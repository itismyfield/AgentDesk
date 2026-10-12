use super::*;
use crate::services::tui_o::repost::admission::{ApprovedRejected, PriorBudget};
use crate::services::tui_o::repost::runner::{ApprovalCheck, Tick};

fn selected(enabled: bool, channels: &[u64]) -> Arc<crate::config::Config> {
    let mut config = crate::config::Config::default();
    config.tui_o = Some(TuiOConfig {
        repost: RepostConfig {
            enabled,
            channels: channels.iter().copied().collect(),
        },
        ..TuiOConfig::default()
    });
    Arc::new(config)
}

fn fresh_record() -> PieceRecord {
    PieceRecord {
        unit_key: unit("allowlist-fresh"),
        piece_index: 0,
        payload: "fresh".into(),
        anchor_id: 100,
        epoch: 1,
        prepared_at: chrono::Utc::now(),
        outcome: None,
    }
}

struct Approved;

impl ApprovalCheck for Approved {
    fn current(&self, _: &str, _: &ApprovedRejected) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn d4b_denied_turn_original_admission_and_tick_have_zero_pg_work_pg() {
    let db = TestPostgresDb::create().await;
    let migrated = db.connect_and_migrate().await;
    migrated.close().await;
    let acquisitions = Arc::new(AtomicUsize::new(0));
    let on_connect = Arc::clone(&acquisitions);
    let on_acquire = Arc::clone(&acquisitions);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .max_lifetime(None)
        .idle_timeout(None)
        .after_connect(move |_, _| {
            on_connect.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        })
        .before_acquire(move |_, _| {
            on_acquire.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(true) })
        })
        .connect(&db.database_url)
        .await
        .unwrap();
    for channels in [Vec::new(), vec![CHANNEL + 1], vec![CHANNEL]] {
        let allowed = channels.contains(&CHANNEL);
        let harness = Harness::new();
        harness.gate.acquired();
        let io = Io::over(&harness.port);
        let (_tx, rx) = watch::channel(Some(selected(true, &channels)));
        let switch = RepostSwitch::new(None, rx);
        let mut hook = turn(&harness, pool.clone(), switch.clone(), &io);
        let mut writer = harness.writer();
        acquisitions.store(0, Ordering::SeqCst);
        assert_eq!(
            <ChannelTurn<Io> as RepostHook<Writer>>::original(&mut hook, 0, fresh_record()).await,
            OriginalGate::Send
        );
        hook.turn(&mut writer).await;
        let mut runner = Runner::new(switch.clone(), pool.clone(), BOT, run_scope());
        let tick = runner
            .tick(CHANNEL, &*io, &[], tokio::time::Instant::now())
            .await
            .unwrap();
        assert_eq!(matches!(tick, Tick::Ran(_)), allowed);
        if !allowed {
            writer.set_live(true);
            writer.bind_switch(switch.live_for(CHANNEL));
            let intent = DispatchIntent::OperatorResume {
                approval_id: "approval".into(),
                approved: ApprovedRejected {
                    rejected_serial: 0,
                    key: key("allowlist-approved"),
                    payload: "approved".into(),
                    anchor: 100,
                },
                prior: PriorBudget::Known { counted_posts: 1 },
                check: &Approved,
            };
            assert!(matches!(
                runner
                    .try_next_dispatch(&mut writer, OriginalGate::Admitted, intent)
                    .await
                    .unwrap(),
                Admission::Parked
            ));
        }
        assert_eq!(
            acquisitions.load(Ordering::SeqCst) > 0,
            allowed,
            "selector={channels:?}"
        );
        assert_eq!(sidecar(&harness).exists(), allowed);
        assert_eq!(io.reads.load(Ordering::SeqCst), 0);
        assert_eq!(io.wire(), 0);
        let mut writer = writer.with_repost(Some(Box::new(hook)));
        assert_eq!(
            writer.deliver(&piece("allowlist-fresh", "fresh")).await,
            deliver::Step::Done
        );
        assert_eq!(harness.port.posts(), ["fresh"]);
        assert_eq!(io.wire(), 0, "zero re-post POSTs permits a normal original");
    }
    pool.close().await;
    db.drop().await;
}

fn reload(path: &std::path::Path, channels: &[u64]) {
    std::fs::write(
        path,
        serde_yaml::to_string(&*selected(true, channels)).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        crate::config_live_reload::reload_from_path(path),
        crate::config_live_reload::ReloadOutcome::Applied { .. }
    ));
}

async fn start_allowed(
    runner: &mut Runner,
    writer: &mut Writer,
    pool: &PgPool,
    key: &PieceKey,
    io: &Io,
) -> dispatch::Started {
    let intent = evidence(pool, key).await;
    let Admission::Granted { permit, lease } = runner
        .try_next_dispatch(writer, OriginalGate::Admitted, intent)
        .await
        .unwrap()
    else {
        panic!("included channel must grant");
    };
    dispatch::start(writer, permit, lease, io).await
}

#[tokio::test]
async fn d4b_validated_membership_revocation_blocks_next_grant_and_wire_pg() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reload.yml");
    reload(&path, &[CHANNEL]);
    let switch = RepostSwitch::new(None, crate::config_live_reload::subscribe());
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    writer.set_live(true);
    writer.bind_switch(switch.live_for(CHANNEL));
    let mut runner = Runner::new(switch, pool.clone(), BOT, run_scope());
    let io = Io::over(&harness.port);
    let granted = admitted(&pool, "membership-granted", "piece").await;
    let next = admitted(&pool, "membership-next", "piece").await;
    let intent = evidence(&pool, &granted).await;
    let Admission::Granted { permit, lease } = runner
        .try_next_dispatch(&mut writer, OriginalGate::Admitted, intent)
        .await
        .unwrap()
    else {
        panic!("included channel must grant");
    };
    reload(&path, &[CHANNEL + 1]);
    assert!(matches!(
        runner
            .try_next_dispatch(
                &mut writer,
                OriginalGate::Admitted,
                evidence(&pool, &next).await
            )
            .await
            .unwrap(),
        Admission::Parked
    ));
    dispatch::start(&mut writer, permit, lease, &*io)
        .await
        .finish(&mut writer, &pool)
        .await
        .unwrap();
    assert_eq!(
        io.wire(),
        0,
        "withdrawn grant must not reach its first wire poll"
    );
    assert_eq!(writer.store().ledger().next_serial(), 0);
    assert_eq!(
        results(&pool, &granted).await,
        [Some(AttemptResult::Uncertain), Some(AttemptResult::NotSent)]
    );
    assert_eq!(
        results(&pool, &next).await,
        [Some(AttemptResult::Uncertain)]
    );
    reload(&path, &[CHANNEL]);
    start_allowed(&mut runner, &mut writer, &pool, &next, &io)
        .await
        .finish(&mut writer, &pool)
        .await
        .unwrap();
    assert_eq!(io.wire(), 1, "re-adding the channel needs no restart");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4b_membership_revocation_keeps_first_poll_and_blocks_429_retry_pg() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reload.yml");
    for (throttle, revoke) in [(false, true), (true, true), (true, false)] {
        reload(&path, &[CHANNEL]);
        let key = admitted(
            &pool,
            &format!("membership-poll-{throttle}-{revoke}"),
            "piece",
        )
        .await;
        let harness = Harness::new();
        harness.gate.acquired();
        let switch = RepostSwitch::new(None, crate::config_live_reload::subscribe());
        let mut writer = harness.writer();
        writer.set_live(true);
        writer.bind_switch(switch.live_for(CHANNEL));
        let mut runner = Runner::new(switch, pool.clone(), BOT, run_scope());
        let io = Io::over(&harness.port);
        io.throttle_first.store(throttle, Ordering::SeqCst);
        io.hold.store(!throttle, Ordering::SeqCst);
        let started = start_allowed(&mut runner, &mut writer, &pool, &key, &io).await;
        io.waiting.notified().await;
        assert_eq!(io.wire(), 1);
        if revoke {
            reload(&path, &[CHANNEL + 1]);
        }
        io.release.notify_one();
        started.finish(&mut writer, &pool).await.unwrap();
        assert_eq!(io.wire(), if throttle && !revoke { 2 } else { 1 });
        let expected = if throttle && revoke {
            AttemptResult::NotSent
        } else {
            AttemptResult::Created
        };
        assert_eq!(
            results(&pool, &key).await,
            [Some(AttemptResult::Uncertain), Some(expected)]
        );
        assert!(writer.store().ledger().unresolved().is_none());
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4b_reload_while_candidate_waits_on_pg_prevents_a_grant_pg() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let db = TestPostgresDb::create().await;
    let migrated = db.connect_and_migrate().await;
    migrated.close().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.database_url)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reload.yml");
    reload(&path, &[CHANNEL]);
    let key = admitted(&pool, "membership-pending-grant", "piece").await;
    let intent = evidence(&pool, &key).await;
    let switch = RepostSwitch::new(None, crate::config_live_reload::subscribe());
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    writer.set_live(true);
    writer.bind_switch(switch.live_for(CHANNEL));
    let mut runner = Runner::new(switch, pool.clone(), BOT, run_scope());
    let connection = pool.acquire().await.unwrap();
    let mut candidate =
        Box::pin(runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent));
    std::future::poll_fn(|cx| {
        assert!(
            candidate.as_mut().poll(cx).is_pending(),
            "the candidate must be in flight"
        );
        std::task::Poll::Ready(())
    })
    .await;
    reload(&path, &[CHANNEL + 1]);
    drop(connection);
    assert!(!matches!(
        candidate.await.unwrap(),
        Admission::Granted { .. }
    ));
    assert_eq!(results(&pool, &key).await, [Some(AttemptResult::Uncertain)]);
    assert_eq!(writer.store().ledger().next_serial(), 0);
    assert!(harness.port.posts().is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test(start_paused = true)]
async fn d4b_off_approved_piece_still_posts_once_through_actor() {
    let (harness, _, _) = switched_over(b"");
    harness.gate.acquired();
    let mut channel = harness.channel();
    channel
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: unit("ledger-only"),
            piece_index: 0,
            payload: "ledger-only payload".into(),
            anchor_id: 100,
            epoch: 1,
        })
        .unwrap();
    channel
        .append_ledger(LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        })
        .unwrap();
    drop(channel);
    harness
        .store
        .record_operator_resume(CHANNEL, 0, "operator", "restored")
        .unwrap();
    let io = Io::over(&harness.port);
    let hook = turn(&harness, unreachable(), switch(false), &io);
    let (stop, task) = spawn(harness.writer().with_repost(Some(Box::new(hook))));
    polls(3).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["ledger-only payload"]);
    assert_eq!(io.wire(), 0);
    assert!(!sidecar(&harness).exists());
}
