use super::*;
use crate::services::discord::catch_up::scan_progress::{self as progress, Origin};

async fn ended_dispatch(case: &str) {
    let mut fx = Fixture::new().await;
    let db =
        crate::dispatch::test_support::DispatchPostgresTestDb::create("dispatch_retirement", case)
            .await;
    let pool = db.connect_and_migrate().await;
    fx.shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    {
        let mut settings = fx.shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }
    for (index, (first_boot, age)) in [(false, 30), (true, 30), (false, 400), (true, 400)]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(7_441_300 + index as u64);
        let message = id(2, age);
        let checkpoint = checkpoint_path(fx.root.path(), &fx.provider, channel);
        std::fs::create_dir_all(checkpoint.parent().unwrap()).unwrap();
        std::fs::write(&checkpoint, message.get().to_string()).unwrap();
        fx.shared.last_message_ids.insert(channel, message.get());
        assert!(!progress::has_file(&fx.provider, channel));
        if !first_boot {
            progress::advance(
                &fx.provider,
                channel,
                message.get() - 1,
                Origin::ObservedFrontier,
            );
        }
        let _owner = (case == "o_delivery").then(|| {
            crate::services::tui_o::cutover::test_override::force_channels(&[(
                channel.get(),
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            )])
        });
        let token = Arc::new(discord::CancelToken::new());
        assert!(
            discord::mailbox_try_start_turn(
                &fx.shared,
                channel,
                token.clone(),
                serenity::UserId::new(HUMAN_ID),
                message
            )
            .await
        );
        if case == "cancelled" {
            token
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let persistence = discord::queue_persistence_context(&fx.shared, &fx.provider, channel);
        fx.shared.mailbox(channel).finish_turn(persistence).await;
        let api = StrictApi::new(&fx.shared).with_history(channel, vec![human(channel, message)]);
        fx.sweep(&api).await;
        assert!(
            accepted(&api).is_empty(),
            "case={case} first_boot={first_boot} age={age}"
        );
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&fx.provider, channel.get()).is_empty(),
            "dispatch cannot prove a delivered body"
        );
        assert!(progress::read(&fx.provider, channel).is_some_and(|after| after >= message.get()));
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_dead_letter")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "ended inputs must not become TooOld losses");
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441a2_cancelled_dispatch_is_retired_without_settlement_pg() {
    ended_dispatch("cancelled").await;
}
#[tokio::test(flavor = "current_thread")]
async fn i7441a2_o_dispatch_is_retired_without_settlement_pg() {
    ended_dispatch("o_delivery").await;
}
#[tokio::test(flavor = "current_thread")]
async fn i7441a2_failed_dispatch_is_retired_without_settlement_pg() {
    ended_dispatch("provider_error_or_empty").await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441a2_unowned_and_inaccessible_remove_sidecars() {
    let fx = Fixture::new().await;
    for (index, (unowned, recent_only, error)) in [
        (true, false, ""),
        (false, false, "Missing Access"),
        (false, false, "Unknown Channel"),
        (false, true, "Missing Access"),
    ]
    .into_iter()
    .enumerate()
    {
        let channel = ChannelId::new(7_441_310 + index as u64);
        let before = id(1, 700);
        fx.seed_checkpoint(channel, before);
        progress::advance(
            &fx.provider,
            channel,
            before.get(),
            Origin::ObservedFrontier,
        );
        let mut api = StrictApi::new(&fx.shared);
        api.unowned = unowned;
        api.fetch_failure = (!unowned).then(|| (recent_only, error.to_string()));
        fx.sweep(&api).await;
        let checkpoint = checkpoint_path(fx.root.path(), &fx.provider, channel);
        assert!(!checkpoint.exists());
        assert!(
            !checkpoint.with_extension("sweep.json").exists(),
            "case={index}"
        );
        assert!(
            !checkpoint.with_extension("sweep.json.lock").exists(),
            "case={index}"
        );
        assert!(
            !super::super::super::collect_catch_up_channel_candidates(
                checkpoint.parent().unwrap(),
                &fx.provider
            )
            .contains_key(&channel.get())
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn i7441a2_dispatch_ring_is_scoped_and_serializes_writers() {
    use crate::services::discord::inject_disposition::dispatched;
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_441_320);
    let mut writers = Vec::new();
    for sequence in 1..=12 {
        let provider = fx.provider.clone();
        writers.push(std::thread::spawn(move || {
            dispatched::record(
                &provider,
                channel,
                MessageId::new(sequence),
                &[MessageId::new(sequence + 100), MessageId::new(1)],
            )
            .unwrap();
        }));
    }
    for writer in writers {
        writer.join().unwrap();
    }
    let ids = dispatched::read(&fx.provider, channel);
    assert_eq!(ids.len(), 24);
    assert!(dispatched::read(&fx.provider, ChannelId::new(channel.get() + 1)).is_empty());
    assert!(dispatched::read(&ProviderKind::Codex, channel).is_empty());
    assert!(completed_turn_ledger::settled_user_msg_ids(&fx.provider, channel.get()).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn i7441a2_failed_or_corrupt_dispatch_proof_rescans() {
    use crate::services::discord::inject_disposition::dispatched;
    let fx = Fixture::new().await;
    for corrupt in [false, true] {
        let channel = ChannelId::new(if corrupt { 7_441_322 } else { 7_441_321 });
        let (before, message) = (id(1, 700), id(2, 30));
        fx.seed_checkpoint(channel, before);
        fx.shared.last_message_ids.insert(channel, message.get());
        let failure = (!corrupt).then(|| dispatched::fail_writes_for_tests(&fx.provider, channel));
        assert!(
            discord::mailbox_try_start_turn(
                &fx.shared,
                channel,
                Arc::new(discord::CancelToken::new()),
                serenity::UserId::new(HUMAN_ID),
                message
            )
            .await
        );
        discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel).await;
        drop(failure);
        if corrupt {
            let path = discord::runtime_store::last_message_root()
                .unwrap()
                .join(fx.provider.as_str())
                .join("dispatched_inputs.json");
            std::fs::write(path, b"torn{").unwrap();
        }
        assert!(dispatched::read(&fx.provider, channel).is_empty());
        let api = StrictApi::new(&fx.shared).with_history(channel, vec![human(channel, message)]);
        fx.sweep(&api).await;
        assert_eq!(accepted(&api), vec![message.get()]);
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&fx.provider, channel.get()).is_empty()
        );
    }
}
