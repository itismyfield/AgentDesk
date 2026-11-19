use super::*;
use crate::services::discord::outbound::{
    completed_turn_ledger, o_delivery_evidence::tests::Fixture,
};
use crate::services::discord::recovery_engine::o_settlement_tests::{arm_takeover, reset_takeover};

#[tokio::test]
async fn i7441b_custody_o_capture_initial_and_late_takeover_uses_original_input() {
    for late in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let f = Fixture::new(
            744187 + u64::from(late),
            ProviderKind::Claude,
            &["custody answer"],
        )
        .await;
        let (http, _guard) = f.http().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let selected = [(
            f.row.channel_id,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        )];
        let _o = crate::services::tui_o::cutover::test_override::force_channels(if late {
            &[]
        } else {
            &selected
        });
        let mut b = f.row.clone();
        b.user_msg_id += 99;
        b.turn_nonce = Some("current B".into());
        crate::services::discord::inflight::save_inflight_state(&b).unwrap();
        let outcome = preserve_or_publish(Handoff {
            shared: &shared,
            provider: &ProviderKind::Claude,
            local: &f.row,
            admitted: None,
            content: &f.body,
            delivery_body: None,
            empty_recovery_notice: None,
            response_sent_offset: 0,
            channel_id: ChannelId::new(f.row.channel_id),
            old_anchor: MessageId::new(f.row.current_msg_id),
            watcher_owner_channel_id: ChannelId::new(f.row.channel_id),
            tmux_last_offset: Some(f.row.last_offset),
            cancelled: false,
            cancel_source: String::new(),
            children: &[],
            dispatch_id: None,
            adk_cwd: None,
            should_complete: false,
            should_fail: false,
            resume_failure: false,
            recovery_retry: false,
        })
        .await;
        assert!(matches!(outcome, Outcome::Deferred { .. }));
        if late {
            arm_takeover(
                f.row.channel_id,
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            );
        }
        let n = crate::services::discord::terminal_delivery_custody::drain_for_test(
            |mut payload, checkpoint| {
                let gateway = crate::services::discord::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    ProviderKind::Claude,
                    None,
                );
                let shared = shared.clone();
                async move {
                    let result =
                        resume_payload_with_gateway(&shared, &gateway, &mut payload, &checkpoint)
                            .await;
                    (payload, result)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(n, 1, "custody must retire under O ownership");
        completed_turn_ledger::o_pending::project(&shared, &ProviderKind::Claude, f.row.channel_id)
            .await;
        let ids =
            completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, f.row.channel_id);
        assert!(ids.contains(&f.row.user_msg_id));
        assert!(!ids.contains(&b.user_msg_id));
        assert_eq!(
            crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                f.row.channel_id
            )
            .unwrap()
            .turn_nonce,
            b.turn_nonce
        );
        reset_takeover();
    }
}

// A refused destination's episode is discarded with one durable reason before any custody
// exists: no raw snapshot is written, so nothing waits for a later resume to scrub it.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_episode_keeps_no_custody_and_holds_one_reason_pg() {
    use crate::db::tui_output_disposition as disposition;
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7651_refused_custody",
        "refused custody",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let f = Fixture::new(765_141, ProviderKind::Claude, &["refused answer"]).await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let kind = crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    let _boot = crate::services::tui_o::cutover::test_override::force_candidates(&[(
        f.row.channel_id,
        kind,
    )]);
    let snapshot = crate::services::tui_o::cutover::boot_snapshot().unwrap();
    snapshot
        .candidate(f.row.channel_id)
        .unwrap()
        .release(f.row.channel_id);
    for _ in 0..2 {
        let outcome = preserve_or_publish(Handoff {
            shared: &shared,
            provider: &ProviderKind::Claude,
            local: &f.row,
            admitted: None,
            content: &f.body,
            delivery_body: None,
            empty_recovery_notice: None,
            response_sent_offset: 0,
            channel_id: ChannelId::new(f.row.channel_id),
            old_anchor: MessageId::new(f.row.current_msg_id),
            watcher_owner_channel_id: ChannelId::new(f.row.channel_id),
            tmux_last_offset: Some(f.row.last_offset),
            cancelled: false,
            cancel_source: String::new(),
            children: &[],
            dispatch_id: None,
            adk_cwd: None,
            should_complete: false,
            should_fail: false,
            resume_failure: false,
            recovery_retry: false,
        })
        .await;
        assert!(matches!(outcome, Outcome::Discarded));
    }
    let kept = crate::services::discord::terminal_delivery_custody::drain_for_test(
        |payload, _| async move { (payload, Ok(true)) },
    )
    .await
    .unwrap();
    assert_eq!(kept, 0, "no raw custody of the refused episode");
    assert_eq!(
        disposition::unposted(&pool).await.unwrap().len(),
        1,
        "one reason"
    );
    assert_eq!(
        f.sends.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no body"
    );
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
fn refused_handoff<'a>(
    shared: &'a Arc<SharedData>,
    local: &'a InflightTurnState,
    body: &'a str,
) -> Handoff<'a> {
    Handoff {
        shared,
        provider: &ProviderKind::Claude,
        local,
        admitted: None,
        content: body,
        delivery_body: None,
        empty_recovery_notice: None,
        response_sent_offset: 0,
        channel_id: ChannelId::new(local.channel_id),
        old_anchor: MessageId::new(local.current_msg_id),
        watcher_owner_channel_id: ChannelId::new(local.channel_id),
        tmux_last_offset: Some(local.last_offset),
        cancelled: false,
        cancel_source: String::new(),
        children: &[],
        dispatch_id: None,
        adk_cwd: None,
        should_complete: false,
        should_fail: false,
        resume_failure: false,
        recovery_retry: false,
    }
}

// A refusal that completes after the custody entry first saw a waiting adoption still keeps no
// custody: the reason lookup is held by a table lock while the refusal lands.
#[cfg(unix)]
#[tokio::test]
async fn a_refusal_landing_after_the_custody_entry_looked_keeps_no_custody_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7651_late_refusal_custody",
        "late refusal custody",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let mut f = Fixture::new(765_142, ProviderKind::Claude, &["refused answer"]).await;
    f.row.turn_nonce = Some("late-refusal-turn".into());
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let kind = crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    let channel = f.row.channel_id;
    let _boot =
        crate::services::tui_o::cutover::test_override::force_candidates(&[(channel, kind)]);
    let mut locker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE tui_output_dispositions IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *locker)
        .await
        .unwrap();
    let refuse = async {
        let waiting = async {
            let blocked = "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' AND datname = current_database()";
            while sqlx::query_scalar::<_, i64>(blocked)
                .fetch_one(&pool)
                .await
                .unwrap()
                == 0
            {
                tokio::task::yield_now().await;
            }
        };
        crate::test_support::wait::await_within(waiting, crate::test_support::wait::WAIT_CAP).await;
        let snapshot = crate::services::tui_o::cutover::boot_snapshot().unwrap();
        snapshot.candidate(channel).unwrap().release(channel);
        locker.commit().await.unwrap();
    };
    let (outcome, ()) = tokio::join!(
        preserve_or_publish(refused_handoff(&shared, &f.row, &f.body)),
        refuse
    );
    assert!(matches!(outcome, Outcome::Discarded));
    let kept = crate::services::discord::terminal_delivery_custody::drain_for_test(
        |payload, _| async move { (payload, Ok(true)) },
    )
    .await
    .unwrap();
    assert_eq!(kept, 0, "no raw custody of the late refused episode");
    let reasons = crate::db::tui_output_disposition::unposted(&pool)
        .await
        .unwrap();
    assert_eq!(reasons.len(), 1, "one reason");
    assert_eq!(
        f.sends.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no body"
    );
    pool.close().await;
    db.drop().await;
}

// An episode holding a reason is closed whatever the destination reads: no new custody, an
// existing one retires on resume, and the first reason stays.
#[cfg(unix)]
#[tokio::test]
async fn a_stored_reason_closes_the_episode_at_custody_whatever_the_gate_reads_pg() {
    use crate::db::tui_output_disposition as disposition;
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
    use crate::services::tui_o::cutover::test_override;
    let cases = [
        "waiting",
        "owned",
        "legacy",
        "unknown kind",
        "kind mismatch",
    ];
    for (channel, case) in (765_150..).zip(cases) {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "i7651_reason_first_custody",
            "reason first custody",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let mut f = Fixture::new(channel, ProviderKind::Claude, &["closed answer"]).await;
        f.row.turn_nonce = Some(format!("closed-turn-{channel}"));
        if case == "unknown kind" {
            f.row.runtime_kind = None;
        }
        let (http, _http) = f.http().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let row_channel = f.row.channel_id;
        let _boot = match case {
            "owned" => test_override::force_channels(&[(row_channel, ClaudeTui)]),
            "legacy" => test_override::force_channels(&[]),
            "kind mismatch" => test_override::force_candidates(&[(row_channel, CodexTui)]),
            _ => test_override::force_candidates(&[(row_channel, ClaudeTui)]),
        };
        // A custody kept before the reason existed, then the episode's first reason.
        let kept = preserve_or_publish(refused_handoff(&shared, &f.row, &f.body)).await;
        assert!(matches!(kept, Outcome::Deferred { .. }), "{case}");
        let refused_discard = crate::services::discord::recovery_engine::refused_discard::identity;
        let id = refused_discard(&ProviderKind::Claude, &f.row).unwrap();
        disposition::persist_intent(&pool, &id, disposition::Reason::Deadline)
            .await
            .unwrap();
        let again = preserve_or_publish(refused_handoff(&shared, &f.row, &f.body)).await;
        assert!(
            matches!(again, Outcome::Discarded),
            "{case}: no new custody"
        );
        let retired = crate::services::discord::terminal_delivery_custody::drain_for_test(
            |mut payload, checkpoint| {
                let gateway = crate::services::discord::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    ProviderKind::Claude,
                    None,
                );
                let shared = shared.clone();
                async move {
                    let result =
                        resume_payload_with_gateway(&shared, &gateway, &mut payload, &checkpoint)
                            .await;
                    assert_eq!(result, Ok(true), "the stored reason retires the custody");
                    (payload, result)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(retired, 1, "{case}: the existing custody retires");
        let first = disposition::record(&pool, &id)
            .await
            .unwrap()
            .map(|(r, _)| r);
        assert_eq!(
            first,
            Some(disposition::Reason::Deadline),
            "{case}: first reason kept"
        );
        assert_eq!(
            f.sends.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{case}: no body"
        );
        pool.close().await;
        db.drop().await;
    }
}

// An episode already refused when the custody write starts keeps no raw custody: that refusal is
// read again under the adoption's lock after the record lock, and nothing is written.
#[cfg(unix)]
#[tokio::test]
async fn a_refusal_before_the_custody_write_keeps_no_raw_custody() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = Fixture::new(765_143, ProviderKind::Claude, &["refused answer"]).await;
    let kind = crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    let channel = f.row.channel_id;
    let _boot =
        crate::services::tui_o::cutover::test_override::force_candidates(&[(channel, kind)]);
    let snapshot = crate::services::tui_o::cutover::boot_snapshot().unwrap();
    snapshot.candidate(channel).unwrap().release(channel);
    let payload = serde_json::json!({"full_response": f.body});
    let kept = persist_unless_refused("refused-entry", &payload, &f.row, channel).await;
    assert_eq!(kept, Ok(true), "the refusal kept the record unwritten");
    let raw = crate::services::discord::terminal_delivery_custody::drain_for_test(
        |payload, _| async move { (payload, Ok(true)) },
    )
    .await
    .unwrap();
    assert_eq!(raw, 0, "no raw custody of the refused episode");
}

// A thread child kept while released with no Legacy run stays kept across a restart whose boot
// does not place it again, with or without a reservation hint: no body, no reason.
#[cfg(unix)]
#[tokio::test]
async fn a_kept_thread_child_is_not_posted_after_a_restart_that_does_not_place_it() {
    use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
    use crate::services::tui_o::cutover::test_override;
    for (case, hinted) in [(0_u64, false), (1, true)] {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let f = Fixture::new(765_150 + case, ProviderKind::Claude, &["child answer"]).await;
        let (http, _guard) = f.http().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let (child, parent) = (f.row.channel_id, 765_160 + case);
        let config =
            serde_json::from_value(serde_json::json!({"server": {}, "agents": [{"id": "a",
            "name": "A", "channels": {"claude": {"id": parent.to_string(), "runtime": "tui"}}}],
            "tui_o": {"writer": {"all_tui": false, "channels": [parent]}}}))
            .unwrap();
        let booted = || {
            BootChannels::validate(&config)
                .unwrap()
                .adopted(Adoption::Committed)
        };
        let surface = serde_json::from_value(serde_json::json!({"id": child.to_string(),
            "parent_id": parent.to_string(), "type": 11, "name": "fixture"}))
        .unwrap();
        let kind = crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
        let surface = crate::services::session_host::surface_policy::VerifiedSurface::from_discord(
            &surface, kind,
        )
        .unwrap();
        let before = booted();
        before.reserve_boot_children(&[(child, parent)]);
        assert!(before.resolve_child(child, surface, false));
        before.output_candidate(child).unwrap().release(child);
        let first = test_override::force_boot(before);
        let outcome = preserve_or_publish(refused_handoff(&shared, &f.row, &f.body)).await;
        assert!(
            matches!(outcome, Outcome::Deferred { .. }),
            "a held child keeps its body"
        );
        drop(first);
        let after = booted();
        if hinted {
            after.reserve_boot_children(&[(child, parent)]);
        }
        let _restarted = test_override::force_boot(after);
        let retired = crate::services::discord::terminal_delivery_custody::drain_for_test(
            |mut payload, checkpoint| {
                let gateway = crate::services::discord::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    ProviderKind::Claude,
                    None,
                );
                let shared = shared.clone();
                async move {
                    let result =
                        resume_payload_with_gateway(&shared, &gateway, &mut payload, &checkpoint)
                            .await;
                    (payload, result)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(retired, 0, "the custody stays");
        let kept = crate::services::discord::terminal_delivery_custody::retains_channel(child);
        assert_eq!(kept, Ok(true));
        assert_eq!(
            f.sends.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no body"
        );
    }
}
