use super::*;
use crate::services::tui_o::store::ledger::LocalRetryMark;

struct ReadyPg {
    runner: Runner,
    pool: PgPool,
    key: PieceKey,
    io: Arc<Io>,
    armed: bool,
    turns: Arc<AtomicUsize>,
}

impl RepostHook<Writer> for ReadyPg {
    fn original(&mut self, _: u64, _: PieceRecord) -> Turned<'_, OriginalGate> {
        self.armed = true;
        Box::pin(std::future::ready(OriginalGate::SendLocal))
    }
    fn receipts(
        &mut self,
    ) -> Turned<'_, crate::services::tui_o::repost::projection::ReceiptOverlay> {
        Box::pin(std::future::ready(
            crate::services::tui_o::repost::projection::ReceiptOverlay::Disabled,
        ))
    }
    fn turn<'a>(&'a mut self, writer: &'a mut Writer) -> Turned<'a, ()> {
        Box::pin(async move {
            if !self.armed {
                return;
            }
            self.turns.fetch_add(1, Ordering::SeqCst);
            writer.set_live(true);
            let intent = evidence(&self.pool, &self.key).await;
            let grant = self.runner.try_next_dispatch(writer, intent).await.unwrap();
            let Admission::Granted { permit, lease } = grant else {
                panic!("ready PG work must dispatch");
            };
            dispatch::start(writer, permit, lease, &*self.io)
                .await
                .finish(writer, &self.pool)
                .await
                .unwrap();
            self.armed = false;
        })
    }
}

#[tokio::test]
async fn local_retry_not_found_poll_holds_ready_pg_and_following_ordinary_work_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for following in [false, true] {
        let (harness, path, _) = switched_over(b"");
        harness.gate.acquired();
        harness.port.say(BOT + 1, "readable");
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Unsent);
        let key = admitted(&pool, &format!("ready-{following}"), "PG piece").await;
        let io = Io::over(&harness.port);
        let turns = Arc::new(AtomicUsize::new(0));
        let hook = ReadyPg {
            runner: Runner::new(pool.clone(), BOT, run_scope()),
            pool: pool.clone(),
            key,
            io: io.clone(),
            armed: false,
            turns: turns.clone(),
        };
        let mut writer = harness.writer();
        writer = writer.with_repost(Some(Box::new(hook)));
        let (stop, task) = spawn(writer);
        append(&path, &row("root", "local payload"));
        if following {
            append(&path, &row("following", "following"));
        }
        for _ in 0..4000 {
            if harness
                .alarms
                .raised
                .lock()
                .unwrap()
                .iter()
                .any(|a| matches!(a, WriterAlarm::NotFound { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            harness
                .alarms
                .raised
                .lock()
                .unwrap()
                .iter()
                .any(|a| matches!(a, WriterAlarm::NotFound { .. }))
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(harness.port.posts(), ["local payload"]);
        assert_eq!(
            (turns.load(Ordering::SeqCst), io.wire()),
            (0, 0),
            "same NotFound poll"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            (turns.load(Ordering::SeqCst), io.wire()),
            (0, 0),
            "delay blocks PG"
        );
        assert_eq!(harness.port.posts(), ["local payload"]);
        tokio::time::sleep(Duration::from_secs(33)).await;
        assert_eq!(io.wire(), 1);
        let posts = harness.port.posts();
        assert_eq!(&posts[..2], ["local payload", "local payload"]);
        if following {
            assert_eq!(posts[2], "following");
        }
        halt(stop, task).await;
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn local_retry_real_hook_durable_intent_and_scope_denial_return_distinct_owners_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for delegated in [false, true] {
        let harness = Harness::new();
        harness.gate.acquired();
        if delegated {
            crate::services::cluster::channel_home::register_for_test(
                CHANNEL,
                Some(crate::db::o_channel_homes::HomeState::Worker),
            );
        }
        let io = Io::over(&harness.port);
        let hook = turn(&harness, pool.clone(), &io);
        let mut writer = harness.writer().with_repost(Some(Box::new(hook)));
        assert_eq!(
            writer.deliver(&piece("scope-owner", "payload")).await,
            Step::Done
        );
        assert_eq!(
            writer.store().ledger().piece(0).unwrap().local_retry,
            delegated.then_some(LocalRetryMark::Budget)
        );
        if delegated {
            assert!(!sidecar(&harness).exists());
        } else {
            let log = ProvenanceLog::open(&sidecar(&harness)).unwrap();
            assert!(log.state().intent(0).is_some());
        }
        assert_eq!((io.reads.load(Ordering::SeqCst), io.wire()), (0, 0));
    }
    pool.close().await;
    db.drop().await;
}
