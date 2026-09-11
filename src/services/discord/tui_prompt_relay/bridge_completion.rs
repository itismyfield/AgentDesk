use super::*;

fn tui_direct_bridge_uncommitted_delivery_error(
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: MessageId,
) -> Option<String> {
    let state = super::super::inflight::load_inflight_state(provider, channel_id.get())?;
    if state.user_msg_id != user_msg_id.get() || state.current_msg_id != current_msg_id.get() {
        return None;
    }
    if state.terminal_delivery_committed {
        return None;
    }
    if !state.full_response.trim().is_empty()
        && state.response_sent_offset >= state.full_response.len()
    {
        return None;
    }

    Some(format!(
        "TUI-direct bridge finished without committed terminal delivery for provider {} channel {} user_msg_id {} current_msg_id {}",
        provider.as_str(),
        channel_id.get(),
        user_msg_id.get(),
        current_msg_id.get()
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn ensure_tui_direct_bridge_delivery_committed(
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: MessageId,
    tmux_session_name: &str,
    lease: &ExternalInputRelayLease,
    prompt_anchor_message_id: Option<u64>,
    streamed: bool,
) -> Result<(), String> {
    let Some(error) = tui_direct_bridge_uncommitted_delivery_error(
        provider,
        channel_id,
        user_msg_id,
        current_msg_id,
    ) else {
        return Ok(());
    };

    if streamed {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(|kind| kind.as_str()).unwrap_or("unknown"),
            current_msg_id = current_msg_id.get(),
            prompt_anchor_message_id,
            error = %error,
            "TUI-direct bridge adapter finished without committed streamed response relay"
        );
    } else {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(|kind| kind.as_str()).unwrap_or("unknown"),
            current_msg_id = current_msg_id.get(),
            prompt_anchor_message_id,
            error = %error,
            "TUI-direct bridge adapter finished without committed response relay"
        );
    }
    Err(error)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn tui_direct_bridge_completion_rejects_uncommitted_matching_inflight() {
        let temp = tempfile::tempdir().expect("temp runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());

        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(88_001);
        let user_msg_id = MessageId::new(88_002);
        let current_msg_id = MessageId::new(88_003);
        let mut lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
        lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
        let output_path = temp.path().join("out.jsonl");
        let state = super::super::build_tui_direct_bridge_inflight_state(
            provider.clone(),
            channel_id,
            user_msg_id,
            current_msg_id,
            "prompt",
            "AgentDesk-codex-test",
            output_path.as_path(),
            0,
            &lease,
        );
        super::super::super::inflight::save_inflight_state(&state).expect("save inflight");

        let error = tui_direct_bridge_uncommitted_delivery_error(
            &provider,
            channel_id,
            user_msg_id,
            current_msg_id,
        )
        .expect("matching uncommitted inflight must be an error");
        assert!(error.contains("without committed terminal delivery"));

        let mut fallback_delivered = state.clone();
        fallback_delivered.full_response = "fallback already delivered".to_string();
        fallback_delivered.response_sent_offset = fallback_delivered.full_response.len();
        super::super::super::inflight::save_inflight_state(&fallback_delivered)
            .expect("save fallback-delivered inflight");
        assert!(
            tui_direct_bridge_uncommitted_delivery_error(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
            )
            .is_none(),
            "fallback delivery that advanced response_sent_offset must not be retried"
        );

        let mut committed = fallback_delivered;
        committed.terminal_delivery_committed = true;
        super::super::super::inflight::save_inflight_state(&committed)
            .expect("save committed inflight");
        assert!(
            tui_direct_bridge_uncommitted_delivery_error(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
            )
            .is_none(),
            "committed terminal delivery must not be reported as placeholder-only failure"
        );
    }

    /// #5464 S2 — `:9`'s ABSENT row is NO EVIDENCE, not failure.
    ///
    /// This is a NEGATIVE probe: it asks whether OUR durable row testifies to a
    /// missed delivery. With no row there is no witness at all — not a witness
    /// for failure — so it folds to `None` and the caller returns `Ok(())`.
    /// That is the landed contract (#5844); `turn_bridge/context.rs:3-8` gives
    /// its basis: "Delivery evidence still comes from the durable row, never
    /// from this signal." `s3t4_finalized_accepts_successor_and_missing_row`
    /// pins the disposition only through the adapter, where it is
    /// indistinguishable from the identity fold at `:10-12` (sibling test), so
    /// a later lane could fail-close one fold and still be green here.
    #[test]
    fn missing_durable_row_is_contracted_as_no_evidence_not_failure() {
        let temp = tempfile::tempdir().expect("temp runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());

        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(88_201);
        let user_msg_id = MessageId::new(88_202);
        let current_msg_id = MessageId::new(88_203);
        let tmux = "AgentDesk-codex-s2-missing";
        let lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
        let root = super::super::super::inflight::inflight_runtime_root()
            .expect("runtime root resolves under the test override");
        let path =
            super::super::super::inflight::inflight_state_path(&root, &provider, channel_id.get());

        // PREMISE (positive half): seed OUR row and prove the probe reaches this
        // root and testifies. Without it the `None` below is indistinguishable
        // from a probe that cannot resolve the runtime root at all.
        let state = super::super::build_tui_direct_bridge_inflight_state(
            provider.clone(),
            channel_id,
            user_msg_id,
            current_msg_id,
            "prompt",
            tmux,
            temp.path().join("out.jsonl").as_path(),
            0,
            &lease,
        );
        super::super::super::inflight::save_inflight_state(&state).expect("save inflight");
        assert!(
            tui_direct_bridge_uncommitted_delivery_error(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
            )
            .is_some(),
            "premise: with OUR uncommitted row present the probe must testify, \
             or the absence case below proves nothing"
        );

        std::fs::remove_file(&path).expect("remove the durable row");
        assert!(
            !path.exists(),
            "premise: the durable row must actually be gone"
        );
        assert!(
            tui_direct_bridge_uncommitted_delivery_error(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
            )
            .is_none(),
            "an ABSENT durable row is no evidence of a missed delivery; \
             promoting it to an error would fail-close every rowless turn"
        );
        assert_eq!(
            ensure_tui_direct_bridge_delivery_committed(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
                tmux,
                &lease,
                None,
                false,
            ),
            Ok(()),
            "…and the caller's disposition for an absent row is Ok — the landed \
             contract s3t4 pins through the adapter"
        );
    }

    /// #5464 S2 — `:10-12`'s FOREIGN row is NO EVIDENCE, not failure.
    ///
    /// A row carrying ANOTHER turn's identity says nothing about THIS turn's
    /// delivery; reading it as failure is "another turn's ACK" in the negative
    /// direction, which `docs/relay-state-contract.md:41` forbids in either
    /// polarity. The absent fold (`:9`) and this one reach the SAME `Ok`, and
    /// that sameness is INTENTIONAL rather than a defect awaiting repair: this
    /// seam's whole input is `(provider, channel_id, user_msg_id,
    /// current_msg_id)` plus the one row on disk, so nothing observable
    /// separates "no witness" from "someone else's witness".
    #[test]
    fn foreign_durable_row_is_contracted_as_no_evidence_not_failure() {
        let temp = tempfile::tempdir().expect("temp runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());

        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(88_301);
        let foreign_user = MessageId::new(88_302);
        let foreign_current = MessageId::new(88_303);
        let tmux = "AgentDesk-codex-s2-foreign";
        let lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));

        let foreign = super::super::build_tui_direct_bridge_inflight_state(
            provider.clone(),
            channel_id,
            foreign_user,
            foreign_current,
            "another turn",
            tmux,
            temp.path().join("out.jsonl").as_path(),
            0,
            &lease,
        );
        super::super::super::inflight::save_inflight_state(&foreign).expect("save inflight");

        // PREMISE: this row, probed with ITS OWN identity, IS an uncommitted
        // delivery error. Without it the `None`s below could come from the
        // committed fold (`:13`) or the delivered fold (`:16-19`) instead of
        // the identity fold (`:10-12`), pinning the wrong line.
        assert!(
            tui_direct_bridge_uncommitted_delivery_error(
                &provider,
                channel_id,
                foreign_user,
                foreign_current,
            )
            .is_some(),
            "premise: the seeded row must be an uncommitted delivery error for \
             its OWN turn, or the identity fold is not what returns None below"
        );

        // `:10` is a DISJUNCTION — either half alone makes the row foreign, so
        // probe both and neither half can hide behind the other.
        for (label, user_msg_id, current_msg_id) in [
            (
                "user_msg_id differs",
                MessageId::new(88_304),
                foreign_current,
            ),
            (
                "current_msg_id differs",
                foreign_user,
                MessageId::new(88_305),
            ),
        ] {
            assert!(
                tui_direct_bridge_uncommitted_delivery_error(
                    &provider,
                    channel_id,
                    user_msg_id,
                    current_msg_id,
                )
                .is_none(),
                "{label}: another turn's row is no evidence about THIS turn"
            );
        }
        assert_eq!(
            ensure_tui_direct_bridge_delivery_committed(
                &provider,
                channel_id,
                MessageId::new(88_304),
                foreign_current,
                tmux,
                &lease,
                None,
                false,
            ),
            Ok(()),
            "…and the caller's disposition is Ok — the same landed contract the \
             absent-row fold receives"
        );
    }
}
