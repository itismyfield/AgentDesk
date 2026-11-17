//! Existing orphan-token periodic recovery keeps its authority beside the queue owner.
// Include as a child of cancel_backstop_e2e_tests so its existing harness helpers are reused.
use super::*;

fn periodic_orphan_refusals(
    h: &RelayE2eHarness,
    active: u64,
) -> Vec<crate::services::observability::events::StructuredEvent> {
    crate::services::observability::events::recent(500)
        .into_iter()
        .filter(|event| event.event_type == "invariant_violation")
        .filter(|event| event.channel_id == Some(h.channel_id.get()))
        .filter(|event| {
            event.payload["code_location"]
                == "src/services/discord/health/relay_auto_heal.rs:apply_orphan_pending_token_cleanup"
                && event.payload["invariant"]
                    == crate::services::observability::LIVE_TURN_PROVEN_BY_PROGRESS_INVARIANT
                && event.payload["details"]["mailbox_active_user_msg_id"].as_u64()
                    == Some(active)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn periodic_orphan_token_refusal_preserves_q_then_cancel_backstop_runs_it_once() {
    if !isolated(concat!(
        module_path!(),
        "::periodic_orphan_token_refusal_preserves_q_then_cancel_backstop_runs_it_once"
    )) {
        return;
    }
    let h = harness().await;
    let base = recent_snowflake_base();
    let (active, queued, stop) = (base | 81, base | 82, base | 83);
    let (held, token) = hold_a_turn(&h, active).await;
    let tmux = CLAUDE.build_tmux_session_name("cancel-backstop-periodic-orphan");
    let pane = ProbePane::new(&h, &tmux);
    pane.pane("dead");
    seed_row(&h, active, &token, None, None);
    h.deliver_user_message(queued, Q).await.unwrap();
    stop_and_abort(&h, &token, held, stop).await;
    held_fire(&h).await;
    assert_eq!(queued_ids(&h).await, vec![queued]);

    token.bind_unmanaged_session_name(&tmux);
    clear_row(&h, active);
    h.shared
        .mailbox(h.channel_id)
        .age_active_turn_for_test(Duration::from_secs(31))
        .await;
    let registry = h.health_registry.as_ref().unwrap();
    let observed = registry
        .snapshot_watcher_state_for_shared(&CLAUDE, h.shared.clone(), h.channel_id.get())
        .await
        .expect("periodic orphan snapshot is measured");
    assert_eq!(
        observed.relay_stall_state,
        discord::relay_health::RelayStallState::OrphanPendingToken
    );
    assert_eq!(observed.relay_health.tmux_alive, Some(false));
    assert!(!observed.relay_health.bridge_inflight_present);
    assert!(!observed.relay_health.watcher_attached);
    assert!(
        observed.reachability_observation().is_none(),
        "the fixture has no axis-B ledger warrant"
    );
    let queue_file = discord::runtime_store::discord_pending_queue_root()
        .expect("isolated queue root")
        .join(CLAUDE.as_str())
        .join(&h.shared.token_hash)
        .join(format!("{}.json", h.channel_id.get()));
    assert!(
        queue_file.is_file(),
        "Q must have an actual durable original"
    );
    let original_queue_bytes = std::fs::read(&queue_file).unwrap();
    let owner = h
        .shared
        .restart
        .deferred_hook_channels
        .get(&h.channel_id)
        .unwrap()
        .value()
        .clone();
    assert!(periodic_orphan_refusals(&h, active).is_empty());

    // Await the production periodic routine, including its fresh orphan-token snapshot and apply.
    for _ in 0..2 {
        let applied = tokio::time::timeout(
            Duration::from_secs(10),
            discord::health::run_orphan_token_auto_heal_pass_for_tests(
                registry,
                &CLAUDE,
                std::slice::from_ref(&h.shared),
            ),
        )
        .await
        .expect("actual periodic pass returns");
        assert_eq!(
            applied, 0,
            "absence alone gives auto-heal no retirement warrant"
        );
        let after = h.mailbox().await;
        assert!(
            after
                .cancel_token
                .as_ref()
                .is_some_and(|now| Arc::ptr_eq(now, &token))
        );
        assert_eq!(
            after.active_user_message_id.map(|id| id.get()),
            Some(active)
        );
        assert_eq!(queued_ids(&h).await, vec![queued]);
        assert_eq!(std::fs::read(&queue_file).unwrap(), original_queue_bytes);
        assert!(h.provider_inputs().is_empty());
        assert!(Arc::ptr_eq(
            &owner,
            h.shared
                .restart
                .deferred_hook_channels
                .get(&h.channel_id)
                .unwrap()
                .value(),
        ));
    }
    let refusals = periodic_orphan_refusals(&h, active);
    assert_eq!(
        refusals.len(),
        1,
        "actual orphan-token branch grades this episode once"
    );
    assert_eq!(
        refusals[0].payload["details"]["refused_reason"],
        "axis_b_orphan_token_reachability_unobserved"
    );
    assert_eq!(refusals[0].payload["details"]["queue_depth"], 1);
    assert_eq!(refusals[0].payload["details"]["retired"], false);
    assert!(!h.root.path().join("tmux-kills").exists());
    assert!(
        pane.writes().is_empty(),
        "neither the sweep nor held backstop types into tmux"
    );

    wake_existing(&h);
    answered_once(&h).await;
    assert_eq!(periodic_orphan_refusals(&h, active).len(), 1);
    assert!(!h.root.path().join("tmux-kills").exists());
}
