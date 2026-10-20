use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
};

#[tokio::test(flavor = "current_thread")]
async fn a_tmux_claim_survives_a_hosted_select_error_while_the_actor_pin_stays_available_pg() {
    Box::pin(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding =
            crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_793);
        let _hosted =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Box::pin(Fixture::new(root.path(), channel)).await;
        let mut record = fixture.pending(N1, &fixture.source, 7_186_794);
        record.hosted_source = None;
        fixture.publish_pending_lease(&record);
        dedupe::herdr_claim_admission(&fixture.logical, true)
            .unwrap()
            .unwrap()
            .remove(&fixture.logical)
            .unwrap();
        let _tmux = crate::config::session_hosts::force_for_test(None, &[]);
        crate::services::tmux_common::host_marker::record_tmux_host_marker(&fixture.logical);
        canonical_fixture::set_raw(&fixture.pool, &fixture.key, None).await;
        assert!(canonical_fixture::raw_of(&fixture.pool, &fixture.key).await.is_none());
        assert_eq!(
            dedupe::hosted_source::herdr_evidence(channel.get(), &fixture.logical, None),
            Ok(false)
        );
        // This isolated database loses only the hosted SELECT column; the existing actor pin
        // must still capture and commit through its ordinary active/status columns.
        sqlx::query("ALTER TABLE sessions RENAME COLUMN hosted_execution TO unavailable_hosted_execution")
            .execute(&fixture.pool)
            .await
            .unwrap();
        assert!(matches!(
            load_hosted_execution_pg(&fixture.pool, HostedLookupKey::SessionKey(&fixture.key)).await,
            HostedLookup::Unknown(why)
                if why.contains("hosted_execution") && why.contains("does not exist")
        ));
        let original_pg_pin = canonical::capture_hook_session_actor_pin_pg(&fixture.pool, &fixture.key)
            .await
            .expect("the existing PG actor pin remains readable before the production claim");
        assert!(matches!(original_pg_pin, canonical::HookSessionActorPin::Existing { .. }));
        assert!(
            fixture.claim(&record).await,
            "a non-Herdr hosted SELECT outage must not deny a production claim whose PG actor pin is available"
        );
        let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
        assert!(actor.cancel_token.is_some());
        assert_eq!(actor.active_user_message_id, Some(MessageId::new(record.anchor_message_id)));
        let saved = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
            .expect("the hosted SELECT outage still allowed an inflight save");
        assert_eq!(saved.user_msg_id, record.anchor_message_id);
        assert_eq!(saved.external_turn_id, record.lease_turn_id);
        assert!(saved.hosted_record_id.is_none());
        assert!(saved.hosted_execution_nonce.is_none());
        assert_eq!(saved.turn_nonce, actor.active_turn_nonce);
        assert_eq!(
            canonical::capture_hook_session_actor_pin_pg(&fixture.pool, &fixture.key)
                .await
                .unwrap(),
            original_pg_pin,
            "synthetic admission preserves the captured PG row until bridge handoff"
        );
        fixture.close().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_retired_hosted_row_allows_tmux_claim_without_weakening_unknown_or_bound_rows_pg() {
    Box::pin(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding =
            crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_791);
        let _hosted =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Box::pin(Fixture::new(root.path(), channel)).await;
        let original = fixture.pending(N1, &fixture.source, 7_186_792);
        fixture.publish_pending_lease(&original);
        let original_pin = original.hosted_source.as_ref().unwrap();
        assert!(original_pin.hosted_admission_required);
        assert!(dedupe::hosted_source::validate(original_pin));
        assert!(matches!(
            load_hosted_execution_pg(&fixture.pool, HostedLookupKey::SessionKey(&fixture.key)).await,
            HostedLookup::Found(ref observed)
                if matches!(&observed.record, HostedRecord::Known(row) if row.state == HostedState::Bound)
        ));

        dedupe::herdr_claim_admission(&fixture.logical, true)
            .unwrap()
            .unwrap()
            .remove(&fixture.logical)
            .expect("the fixture's original admitted Herdr execution was listed");
        let _tmux = crate::config::session_hosts::force_for_test(None, &[]);
        crate::services::tmux_common::host_marker::record_tmux_host_marker(&fixture.logical);
        assert!(!dedupe::herdr_execution_listed(&fixture.logical));
        assert_eq!(
            dedupe::hosted_source::herdr_evidence(channel.get(), &fixture.logical, None),
            Ok(false)
        );
        let mut tmux_input = original.clone();
        tmux_input.hosted_source = None;
        assert!(tmux_input.hosted_source_checked);

        let bound = fixture.hosted_json().await;
        assert!(
            !fixture.claim(&tmux_input).await,
            "a live Bound hosted row remains strict after current host hints are lost"
        );
        fixture.assert_quiet().await;
        assert_eq!(fixture.hosted_json().await, bound);

        canonical_fixture::set_raw(&fixture.pool, &fixture.key, Some(serde_json::json!({
            "schema": 999
        })))
        .await;
        assert!(matches!(
            load_hosted_execution_pg(&fixture.pool, HostedLookupKey::SessionKey(&fixture.key)).await,
            HostedLookup::Found(ref observed) if matches!(&observed.record, HostedRecord::Unknown(_))
        ));
        assert!(
            !fixture.claim(&tmux_input).await,
            "an unreadable hosted record is not a non-Herdr lookup outage"
        );
        fixture.assert_quiet().await;

        let mut wrong_owner = fixture.execution.clone();
        wrong_owner.owner.channel_id = (channel.get() + 10).to_string();
        wrong_owner.source_ref.channel = wrong_owner.owner.channel_id.clone();
        canonical_fixture::set_raw(
            &fixture.pool,
            &fixture.key,
            Some(serde_json::to_value(wrong_owner).unwrap()),
        )
        .await;
        assert!(matches!(
            load_hosted_execution_pg(&fixture.pool, HostedLookupKey::SessionKey(&fixture.key)).await,
            HostedLookup::Conflict(_)
        ));
        assert!(
            !fixture.claim(&tmux_input).await,
            "an ownership Conflict must never use the non-Herdr availability fallback"
        );
        fixture.assert_quiet().await;

        let retired = fixture.replace_execution(HostedState::Retired, N1).await;
        assert!(
            !fixture.claim(&original).await,
            "the original required Herdr pin stays strict even after its row is Retired"
        );
        fixture.assert_quiet().await;
        assert_eq!(
            fixture.hosted_json().await,
            serde_json::to_value(&retired).unwrap()
        );
        assert!(
            fixture.claim(&tmux_input).await,
            "a Retired row alone must not deny a current known tmux production claim"
        );
        let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
        assert!(actor.cancel_token.is_some());
        assert_eq!(
            actor.active_user_message_id,
            Some(MessageId::new(tmux_input.anchor_message_id))
        );
        let saved = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
            .expect("the Retired-to-tmux production claim saved its inflight");
        assert_eq!(saved.user_msg_id, tmux_input.anchor_message_id);
        assert_eq!(saved.external_turn_id, tmux_input.lease_turn_id);
        assert_eq!(saved.output_path.as_deref(), fixture.source.path.to_str());
        assert!(saved.hosted_record_id.is_none());
        assert!(saved.hosted_execution_nonce.is_none());
        assert_eq!(fixture.hosted_json().await, serde_json::to_value(retired).unwrap());
        fixture.close().await;
    })
    .await;
}
