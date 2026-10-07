use super::*;

type StoredDeadLetter = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);

#[derive(Clone)]
struct TooOldLogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for TooOldLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn production_sweep_too_old_drops_leave_no_outbox_notice_pg() {
    let root = scoped_runtime_root();
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_catch_up_no_too_old_notice",
        "catch-up drops without resend notices",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_004);
    let first_id = message_id_with_age(1, Duration::from_secs(410));
    let second_id = message_id_with_age(2, Duration::from_secs(400));
    write_checkpoint(root.path(), &provider, channel_id, first_id.get() - 1);
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allowed_user_ids = vec![HUMAN_ID];
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = TooOldLogWriter(Arc::clone(&logs));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _guard = tracing::subscriber::set_default(subscriber);

    for (index, (id, content)) in [
        (first_id, "첫 사용자 요청"),
        (first_id, "첫 사용자 요청"),
        (second_id, "새 사용자 요청"),
    ]
    .into_iter()
    .enumerate()
    {
        let api = TestCatchUpApi::new(vec![discord_message(
            channel_id, id, HUMAN_ID, false, content,
        )]);
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
        assert!(
            super::super::mailbox_snapshot(&shared, channel_id)
                .await
                .intervention_queue
                .is_empty(),
            "too-old input must be dropped without recovery"
        );
        assert_eq!(
            shared.last_message_ids.get(&channel_id).map(|id| *id),
            Some(id.get())
        );
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_dead_letter")
                    .fetch_one(&pool)
                    .await
                    .expect("count dead letters");
                if count == (index + 1) as i64 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached DLQ producer must finish");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .expect("count outbox rows");
        assert_eq!(
            count, 0,
            "too-old drops must enqueue no notice, including repeat and new batches"
        );
    }
    let records: Vec<StoredDeadLetter> = sqlx::query_as(
        "SELECT kind, channel_id, author_id, message_id, content, reason FROM relay_dead_letter ORDER BY id",
    ).fetch_all(&pool).await.expect("load dead letters");
    assert_eq!(
        records,
        [
            (first_id, "첫 사용자 요청", 410),
            (first_id, "첫 사용자 요청", 410),
            (second_id, "새 사용자 요청", 400),
        ]
        .into_iter()
        .map(|(id, content, age)| (
            crate::db::relay_dead_letter::KIND_CATCH_UP_TOO_OLD.to_string(),
            channel_id.to_string(),
            Some(HUMAN_ID.to_string()),
            Some(id.get().to_string()),
            content.to_string(),
            format!("age_secs={age} > max_age_secs=300"),
        ))
        .collect::<Vec<_>>()
    );
    let logs =
        String::from_utf8(logs.lock().expect("log capture lock").clone()).expect("utf8 logs");
    let warning = logs
        .lines()
        .find(|line| line.contains("catch-up: dropped too-old message(s)"))
        .expect("drop warning");
    assert!(
        warning.contains("WARN") && warning.contains("too_old=1"),
        "{logs}"
    );
    assert!(
        logs.contains("too_old=1") && logs.contains("recovered=0"),
        "{logs}"
    );
    assert!(!logs.contains("aggregate resend notice enqueued"), "{logs}");
    pool.close().await;
    pg_db.drop().await;
}
