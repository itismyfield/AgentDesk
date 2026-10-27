use super::*;
use crate::services::discord::catch_up::scan_progress::{self as progress, Origin};

#[tokio::test(flavor = "current_thread")]
async fn i7441_live_intake_is_never_saved_as_scanner_progress() {
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_114);
    let (observed, unseen) = (id(1, 700), id(2, 30));
    write_role_map(fx.root.path(), &fx.provider, channel);
    fx.shared.last_message_ids.insert(channel, unseen.get());
    let api = StrictApi::new(&fx.shared).with_history(channel, vec![own_reply(channel, observed)]);
    fx.sweep(&api).await;
    assert_eq!(progress::read(&fx.provider, channel), Some(observed.get()));
    assert_eq!(
        fx.shared.last_message_ids.get(&channel).map(|id| *id),
        Some(unseen.get())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_malformed_scanner_without_intake_still_scans_recent() {
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_113);
    let (before, fresh) = (id(1, 700), id(2, 30));
    progress::advance(
        &fx.provider,
        channel,
        before.get(),
        Origin::ObservedFrontier,
    );
    let path = checkpoint_path(fx.root.path(), &fx.provider, channel).with_extension("sweep.json");
    std::fs::write(path, b"truncated{").unwrap();
    let api = StrictApi::new(&fx.shared).with_history(
        channel,
        vec![own_reply(channel, before), human(channel, fresh)],
    );
    fx.sweep(&api).await;
    assert_phase1_read(&api, CatchUpFetchRequest::new(50), &[before, fresh]);
    assert_eq!(accepted(&api), vec![fresh.get()]);
    assert_eq!(progress::read(&fx.provider, channel), Some(fresh.get()));
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_last_item_dedup_retirement_keeps_time_window_and_no_completion() {
    let fx = Fixture::new().await;
    for gap in [10, 11] {
        let channel = ChannelId::new(7_441_120 + gap);
        let incoming = id(2, 30);
        let queued = MessageId::new(incoming.get() - ((gap * 1000) << 22));
        fx.seed_checkpoint(channel, queued);
        let mut intervention = queued_intervention(queued, 0);
        intervention.text = "same request".into();
        let outcome =
            discord::mailbox_enqueue_intervention(&fx.shared, &fx.provider, channel, intervention)
                .await;
        assert!(outcome.enqueued);
        let api = StrictApi::new(&fx.shared).with_history(
            channel,
            vec![discord_message(
                channel,
                incoming,
                HUMAN_ID,
                false,
                "same request",
            )],
        );
        fx.sweep(&api).await;
        assert!(
            api.enqueue_log()
                .iter()
                .any(|(id, accepted, refusal)| *id == incoming.get()
                    && !accepted
                    && *refusal == Some(EnqueueRefusalReason::LastItemDedup))
        );
        let expected = if gap <= 10 { incoming } else { queued };
        assert_eq!(
            progress::read(&fx.provider, channel),
            Some(expected.get()),
            "gap={gap}"
        );
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&fx.provider, channel.get()).is_empty()
        );
    }
}
#[tokio::test(flavor = "current_thread")]
async fn i7441_no_pool_too_old_sweep_does_not_retire_unpreserved_input() {
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_112);
    let (before, old, fresh) = (id(1, 700), id(2, 400), id(3, 30));
    fx.seed_checkpoint(channel, before);
    let api = StrictApi::new(&fx.shared)
        .with_history(channel, vec![human(channel, old), human(channel, fresh)]);
    fx.sweep(&api).await;
    assert_eq!(progress::read(&fx.provider, channel), Some(before.get()));
    assert!(fx.pending(channel).unwrap().checkpoint < old.get());
    assert!(accepted(&api).is_empty());
}
#[tokio::test(flavor = "current_thread")]
async fn i7441_upgrade_does_not_seed_scanner_from_legacy_intake() {
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_110);
    let (before, missed, intake) = (id(1, 700), id(2, 60), id(3, 30));
    write_role_map(fx.root.path(), &fx.provider, channel);
    let path = checkpoint_path(fx.root.path(), &fx.provider, channel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, intake.to_string()).unwrap();
    fx.shared.last_message_ids.insert(channel, intake.get());
    let api = StrictApi::new(&fx.shared).with_history(
        channel,
        vec![own_reply(channel, before), human(channel, missed)],
    );
    fx.sweep(&api).await;
    assert_phase1_read(&api, CatchUpFetchRequest::new(50), &[before, missed]);
    assert_eq!(accepted(&api), vec![missed.get()]);
    assert_eq!(progress::read(&fx.provider, channel), Some(missed.get()));
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_scanner_identity_validation_monotonic_merge_and_prune() {
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_111);
    let low = id(1, 60).get();
    let high = low + 10;
    progress::advance(&fx.provider, channel, high, Origin::ExplicitDiscard);
    progress::advance(&fx.provider, channel, low, Origin::ObservedFrontier);
    assert_eq!(progress::read(&fx.provider, channel), Some(high));
    let path = checkpoint_path(fx.root.path(), &fx.provider, channel).with_extension("sweep.json");
    let valid: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (key, invalid) in [
        ("version", serde_json::json!(2)),
        ("provider", serde_json::json!("codex")),
        ("channel_id", serde_json::json!(channel.get() + 1)),
        ("after", serde_json::json!(0)),
    ] {
        let mut broken = valid.clone();
        broken[key] = invalid;
        std::fs::write(&path, serde_json::to_vec(&broken).unwrap()).unwrap();
        assert_eq!(progress::read(&fx.provider, channel), None, "{key}");
    }
    std::fs::write(&path, b"truncated{").unwrap();
    assert_eq!(progress::read(&fx.provider, channel), None);
    progress::advance(&fx.provider, channel, low, Origin::RecentLowerBound);
    let threads = (0..12)
        .map(|index| {
            std::thread::spawn(move || {
                progress::advance(
                    &ProviderKind::Claude,
                    channel,
                    low + index,
                    Origin::ObservedFrontier,
                )
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(progress::read(&fx.provider, channel), Some(low + 11));
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(601))
        .unwrap();
    super::super::super::prune_stale_checkpoint_files(
        path.parent().unwrap(),
        Duration::from_secs(600),
    );
    assert!(path.exists());
}

fn saved_after(root: &std::path::Path, provider: &ProviderKind, channel: ChannelId) -> u64 {
    let path = checkpoint_path(root, provider, channel).with_extension("sweep.json");
    serde_json::from_slice::<serde_json::Value>(
        &std::fs::read(path).expect("scanner progress must persist"),
    )
    .unwrap()["after"]
        .as_u64()
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_scanner_progress_survives_restart_and_newer_intake_checkpoint() {
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_100);
    let old = id(1, 700);
    write_role_map(fx.root.path(), &fx.provider, channel);
    let api = StrictApi::new(&fx.shared).with_history(channel, vec![own_reply(channel, old)]);
    fx.sweep(&api).await;
    assert_eq!(
        saved_after(fx.root.path(), &fx.provider, channel),
        old.get()
    );
    std::fs::File::open(checkpoint_path(fx.root.path(), &fx.provider, channel))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(601))
        .unwrap();
    let fresh = id(2, 30);
    let shared = discord::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }
    // Intake advanced its checkpoint but lost the input before queue commit or admission.
    shared.last_message_ids.insert(channel, fresh.get());
    let api = StrictApi::new(&shared).with_history(
        channel,
        vec![own_reply(channel, old), human(channel, fresh)],
    );
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &fx.provider)).await;
    assert_phase1_read(&api, after(old), &[fresh]);
    assert_eq!(accepted(&api), vec![fresh.get()]);
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_scanner_progress_stops_at_queued_barrier_before_later_accept() {
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_101);
    let (before, pending, later, fresh) = (id(1, 700), id(2, 90), id(3, 60), id(4, 30));
    write_role_map(fx.root.path(), &fx.provider, channel);
    fx.shared.last_message_ids.insert(channel, fresh.get() + 1);
    fx.queue(channel, pending).await;
    let api = StrictApi::new(&fx.shared).with_history(
        channel,
        vec![
            own_reply(channel, before),
            human(channel, pending),
            own_reply(channel, later),
            human(channel, fresh),
        ],
    );
    fx.sweep(&api).await;
    assert_eq!(accepted(&api), vec![fresh.get()]);
    assert_eq!(
        saved_after(fx.root.path(), &fx.provider, channel),
        before.get()
    );
    assert!(fx.pending(channel).unwrap().checkpoint < pending.get());
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_injection_retirement_outlives_ring_without_completed_entry() {
    use crate::services::discord::inject_disposition::{self, InjectionOutcome};
    let fx = Fixture::new().await;
    for (index, outcome) in [InjectionOutcome::Observed, InjectionOutcome::Unconfirmed]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(7_441_102 + index as u64);
        let message = id(index as u64 + 1, 60);
        fx.seed_checkpoint(channel, id(0, 700));
        inject_disposition::record_terminal(
            &fx.provider,
            channel,
            message,
            outcome,
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
        let api = StrictApi::new(&fx.shared).with_history(channel, vec![human(channel, message)]);
        fx.sweep(&api).await;
        assert_eq!(
            saved_after(fx.root.path(), &fx.provider, channel),
            message.get()
        );
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&fx.provider, channel.get()).is_empty()
        );
        inject_disposition::record_terminal(
            &fx.provider,
            channel,
            message,
            outcome,
            chrono::Utc::now().timestamp_millis() - inject_disposition::DISK_TTL_MS - 1,
        )
        .unwrap();
        let shared = discord::make_shared_data_for_tests();
        shared.settings.write().await.allow_all_users = true;
        shared.settings.write().await.owner_user_id = Some(OWNER_ID);
        let api = StrictApi::new(&shared).with_history(
            channel,
            vec![own_reply(channel, id(0, 700)), human(channel, message)],
        );
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &fx.provider)).await;
        assert!(accepted(&api).is_empty());
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&fx.provider, channel.get()).is_empty()
        );
    }
}
