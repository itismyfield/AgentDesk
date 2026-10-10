use super::receipt::Disposition;
use super::write::{self, BeginFrom, CanonicalInput, EffectProjection, ExactAttempt, WriteError};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::replay_policy::live::{Activity, RawObservation, Seen, TerminalKind};
use sqlx::PgPool;

const TARGET: &str = r#"["provider_start","claude","chan"]"#;

async fn setup() -> (TestPostgresDb, PgPool) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    (fixture, pool)
}

async fn finish(fixture: TestPostgresDb, pool: PgPool) {
    pool.close().await;
    fixture.drop().await;
}

fn input(key: &str, channel: &str, sources: &[&str]) -> CanonicalInput {
    CanonicalInput {
        request_key: key.into(),
        provider: "claude".into(),
        channel: channel.into(),
        sources: sources.iter().map(|source| source.to_string()).collect(),
        original_text: format!("original request {key}"),
        owner_id: "user".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([{ "id": "attachment-1" }]),
        reply_context: Some("reply context".into()),
        provenance: "new_input",
    }
}

fn effect(binding: &serde_json::Value) -> EffectProjection<'_> {
    EffectProjection {
        effect_target: TARGET,
        input_hash: "prepared-input",
        binding,
    }
}

async fn register(pool: &PgPool, request: &CanonicalInput) -> (i64, String) {
    let receipt = write::register_or_reuse(pool, request, "node-a")
        .await
        .expect("register receipt");
    assert_eq!(receipt.disposition, Disposition::RegisteredNotStarted);
    (receipt.id, receipt.episode_nonce.expect("registered nonce"))
}

async fn start(pool: &PgPool, request: &CanonicalInput) -> ExactAttempt {
    let (id, nonce) = register(pool, request).await;
    let binding = serde_json::json!({ "owner": "owner-a" });
    let from = BeginFrom::Registered { nonce: &nonce };
    let ack = write::begin(pool, id, from, &effect(&binding), "incarnation-a")
        .await
        .expect("begin registered attempt");
    ack.into_parts().0
}

fn observed(attempt: &ExactAttempt, seen: &[Seen]) -> RawObservation {
    let mut observation = RawObservation::for_attempt(attempt);
    for item in seen {
        observation.record(*item);
    }
    observation
}

const COMPLETE: [Seen; 3] = [
    Seen::StdoutEof,
    Seen::Exited { waited: true },
    Seen::StderrDrained,
];

async fn receipt_row(
    pool: &PgPool,
    id: i64,
) -> (String, String, Option<String>, serde_json::Value) {
    sqlx::query_as(
        "SELECT replay_disposition, replay_episode_nonce, replay_retry_of_nonce, replay_preserved
           FROM intake_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read receipt")
}

fn fenced(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>, operation: &str) {
    let error = result.expect_err(operation);
    let constraint = error.as_database_error().and_then(|db| db.constraint());
    assert_eq!(
        constraint,
        Some("replay_disposition_fence"),
        "{operation}: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn exact_attempt_migration_preserves_0136_rows_and_reapplies_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = crate::db::postgres::connect_test_pool_with_max_connections(
        &fixture.database_url,
        "replay exact migration",
        4,
    )
    .await
    .expect("connect without migrating");
    let all = sqlx::migrate!("./migrations/postgres");
    let through_0136 = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= 136).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    through_0136.run(&pool).await.expect("migrate through 0136");
    let mut seeded = Vec::new();
    for (n, disposition) in [
        "registered_not_started",
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ]
    .into_iter()
    .enumerate()
    {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id, provider, status,
                replay_only, replay_disposition, replay_source_message_ids, replay_episode_nonce,
                replay_request_key, replay_hold_reason, replay_preserved)
             VALUES ('n', 'n', 'legacy', $1, 'u', 'text', 'foreground', 'a', 'claude', 'unknown',
                     TRUE, $2, ARRAY[$1], 'episode-0136', $1, 'reason', '{\"body\":\"kept\"}')
             RETURNING id",
        )
        .bind(format!("legacy-{n}"))
        .bind(disposition)
        .fetch_one(&pool)
        .await
        .expect("seed 0136 receipt");
        seeded.push((id, disposition));
    }
    sqlx::query(
        "INSERT INTO sessions (session_key, channel_id, provider, status, current_replay_receipt_id,
                               replay_episode_nonce, claude_session_id)
         VALUES ('legacy-session', 'legacy', 'claude', 'turn_active', $1, 'episode-0136', 'resume')",
    )
    .bind(seeded[1].0)
    .execute(&pool)
    .await
    .expect("seed held session");

    crate::db::postgres::migrate(&pool)
        .await
        .expect("apply 0137");
    crate::db::postgres::migrate(&pool)
        .await
        .expect("re-run migrator");
    sqlx::raw_sql(include_str!(
        "../../../migrations/postgres/0137_request_replay_exact_attempt.sql"
    ))
    .execute(&pool)
    .await
    .expect("0137 re-executes in place");
    let triggers: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger WHERE tgname = 'trg_intake_outbox_replay_exact_fence'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(triggers, 1);
    for (id, disposition) in &seeded {
        let (stored, nonce, retry_of, preserved) = receipt_row(&pool, *id).await;
        assert_eq!(
            (stored.as_str(), nonce.as_str(), retry_of),
            (*disposition, "episode-0136", None)
        );
        assert_eq!(preserved, serde_json::json!({ "body": "kept" }));
    }
    fenced(
        sqlx::query(
            "UPDATE sessions SET claude_session_id = NULL WHERE session_key = 'legacy-session'",
        )
        .execute(&pool)
        .await,
        "0136 held session clear stays refused",
    );
    finish(fixture, pool).await;
}

#[tokio::test(flavor = "current_thread")]
async fn begin_commits_one_start_per_registered_attempt_pg() {
    let (fixture, pool) = setup().await;
    let request = input("request-once", "chan", &["m1", "m2"]);
    let (id, nonce) = register(&pool, &request).await;
    assert_eq!(
        register(&pool, &request).await,
        (id, nonce.clone()),
        "same key reuses"
    );
    let mut changed = input("request-once", "chan", &["m1", "m2"]);
    changed.original_text = "a different request".into();
    assert!(matches!(
        write::register_or_reuse(&pool, &changed, "node-a").await,
        Err(WriteError::IdentityConflict(conflict)) if conflict == id
    ));

    let binding = serde_json::json!({ "owner": "owner-a" });
    start(&pool, &input("started-elsewhere", "chan", &["m9"])).await;
    let overlapping = input("overlapping", "chan", &["m3", "m9"]);
    assert!(
        matches!(
            write::register_or_reuse(&pool, &overlapping, "node-a").await,
            Err(WriteError::Fenced(message)) if message.contains("already started")
        ),
        "a source of a started request cannot be registered again"
    );
    let wrong = BeginFrom::Registered {
        nonce: "not-the-nonce",
    };
    assert!(matches!(
        write::begin(&pool, id, wrong, &effect(&binding), "incarnation-a").await,
        Err(WriteError::CasMiss)
    ));
    let mut outcomes = Vec::new();
    for incarnation in ["incarnation-a", "incarnation-b"] {
        let from = BeginFrom::Registered { nonce: &nonce };
        outcomes.push(write::begin(&pool, id, from, &effect(&binding), incarnation).await);
    }
    let first = outcomes.remove(0).expect("first begin commits");
    assert!(
        matches!(outcomes.remove(0), Err(WriteError::CasMiss)),
        "second begin must miss"
    );
    let (attempt, target, input_hash) = first.into_parts();
    assert_eq!(
        (attempt.receipt_id(), attempt.nonce(), attempt.retry_of()),
        (id, nonce.as_str(), None)
    );
    assert_eq!(
        (target.as_str(), input_hash.as_str()),
        (TARGET, "prepared-input")
    );
    let (disposition, _, _, preserved) = receipt_row(&pool, id).await;
    assert_eq!(disposition, "started_unclassified");
    assert_eq!(preserved["projection"]["nonce"], nonce.as_str());
    assert_eq!(preserved["projection"]["binding"], binding);
    assert_eq!(preserved["provenance"], "new_input");
    let incarnation: String =
        sqlx::query_scalar("SELECT replay_owner_incarnation FROM intake_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(incarnation, "incarnation-a");
    finish(fixture, pool).await;
}

#[tokio::test(flavor = "current_thread")]
async fn database_refuses_inexact_attempt_transitions_pg() {
    let (fixture, pool) = setup().await;
    let (registered, _) = register(&pool, &input("registered", "chan", &["r1"])).await;
    let started = start(&pool, &input("started", "chan", &["s1"])).await;
    let no_effect = start(&pool, &input("no-effect", "chan", &["n1"])).await;
    let no_effect_id = no_effect.receipt_id();
    let evidence = observed(&no_effect, &COMPLETE)
        .no_effect_evidence()
        .unwrap();
    write::classify_no_effect(&pool, no_effect, &evidence)
        .await
        .unwrap();
    let id = started.receipt_id();
    let refusals: [(&str, i64); 7] = [
        (
            "SET replay_disposition = 'started_unclassified', replay_owner_incarnation = 'i',
                 replay_episode_nonce = 'moved',
                 replay_preserved = '{\"projection\":{\"nonce\":\"moved\"}}'",
            registered,
        ),
        (
            "SET replay_disposition = 'classified_normal', replay_episode_nonce = 'moved'",
            id,
        ),
        (
            "SET replay_disposition = 'withheld', replay_preserved = NULL",
            id,
        ),
        (
            "SET replay_preserved = replay_preserved || '{\"projection\":{\"nonce\":\"x\"}}'",
            id,
        ),
        ("SET replay_retry_of_nonce = 'invented'", id),
        (
            "SET replay_disposition = 'started_unclassified', replay_episode_nonce = 'next',
                 replay_retry_of_nonce = 'not-the-previous',
                 replay_preserved = '{\"projection\":{\"nonce\":\"next\"}}'",
            no_effect_id,
        ),
        (
            "SET replay_disposition = 'started_unclassified', replay_episode_nonce = 'next',
                 replay_retry_of_nonce = replay_episode_nonce,
                 replay_preserved = '{\"projection\":{\"nonce\":\"other\"}}'",
            no_effect_id,
        ),
    ];
    for (set, row) in refusals {
        let sql = format!("UPDATE intake_outbox {set} WHERE id = $1");
        fenced(sqlx::query(&sql).bind(row).execute(&pool).await, &sql);
    }
    sqlx::query("UPDATE intake_outbox SET replay_disposition = 'classified_normal' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("an exact classification keeps passing");
    sqlx::query(
        "UPDATE intake_outbox SET replay_disposition = 'started_unclassified',
                replay_episode_nonce = 'next', replay_retry_of_nonce = replay_episode_nonce,
                replay_preserved = '{\"projection\":{\"nonce\":\"next\"}}' WHERE id = $1",
    )
    .bind(no_effect_id)
    .execute(&pool)
    .await
    .expect("an exact retry keeps passing");
    finish(fixture, pool).await;
}

#[tokio::test(flavor = "current_thread")]
async fn exact_retry_moves_session_off_stale_resume_once_pg() {
    let (fixture, pool) = setup().await;
    let attempt = start(&pool, &input("retry", "chan", &["t1"])).await;
    let (id, previous) = (attempt.receipt_id(), attempt.nonce().to_string());
    sqlx::query(
        "INSERT INTO sessions (session_key, channel_id, provider, status, current_replay_receipt_id,
                               replay_episode_nonce, claude_session_id, raw_provider_session_id)
         VALUES ('retry-session', 'chan', 'claude', 'turn_active', $1, $2, 'stale', 'stale')",
    )
    .bind(id)
    .bind(&previous)
    .execute(&pool)
    .await
    .unwrap();
    let clear = "UPDATE sessions SET claude_session_id = NULL WHERE session_key = 'retry-session'";
    fenced(
        sqlx::query(clear).execute(&pool).await,
        "general clear of a started session",
    );
    let evidence = observed(&attempt, &COMPLETE).no_effect_evidence().unwrap();
    let (acked_id, acked_previous) = write::classify_no_effect(&pool, attempt, &evidence)
        .await
        .expect("commit no-effect")
        .into_parts();
    assert_eq!((acked_id, acked_previous.as_str()), (id, previous.as_str()));

    let binding = serde_json::json!({ "owner": "owner-a" });
    let retry = |session_key| BeginFrom::NoEffect {
        previous: &previous,
        next: "next-nonce",
        session_key,
    };
    assert!(matches!(
        write::begin(
            &pool,
            id,
            retry("other-session"),
            &effect(&binding),
            "incarnation-a"
        )
        .await,
        Err(WriteError::CasMiss)
    ));
    let (disposition, nonce, _, _) = receipt_row(&pool, id).await;
    assert_eq!(
        (disposition.as_str(), nonce.as_str()),
        ("startup_failed_no_effect", previous.as_str())
    );

    let ack = write::begin(
        &pool,
        id,
        retry("retry-session"),
        &effect(&binding),
        "incarnation-a",
    )
    .await
    .expect("exact retry commits");
    assert_eq!(ack.into_parts().0.retry_of(), Some(previous.as_str()));
    let (disposition, nonce, retry_of, _) = receipt_row(&pool, id).await;
    assert_eq!(
        (disposition.as_str(), nonce.as_str(), retry_of.as_deref()),
        (
            "started_unclassified",
            "next-nonce",
            Some(previous.as_str())
        )
    );
    let session: (Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT claude_session_id, raw_provider_session_id, replay_episode_nonce
           FROM sessions WHERE session_key = 'retry-session'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(session, (None, None, "next-nonce".to_string()));
    for later in [
        "UPDATE sessions SET claude_session_id = 'other' WHERE session_key = 'retry-session'",
        "UPDATE sessions SET replay_episode_nonce = 'again' WHERE session_key = 'retry-session'",
    ] {
        fenced(sqlx::query(later).execute(&pool).await, later);
    }
    assert!(matches!(
        write::begin(
            &pool,
            id,
            retry("retry-session"),
            &effect(&binding),
            "incarnation-a"
        )
        .await,
        Err(WriteError::CasMiss)
    ));
    finish(fixture, pool).await;
}

#[tokio::test(flavor = "current_thread")]
async fn live_evidence_requires_a_complete_observation_and_one_provider_terminal_pg() {
    let (fixture, pool) = setup().await;
    let attempt = start(&pool, &input("evidence", "chan", &["e1"])).await;
    let done = Seen::Terminal(TerminalKind::Done);
    let error = Seen::Terminal(TerminalKind::Error);
    let incomplete: [&[Seen]; 4] = [
        &[Seen::StdoutEof, Seen::Exited { waited: true }, done],
        &[Seen::StdoutEof, Seen::StderrDrained, done],
        &[Seen::Exited { waited: true }, Seen::StderrDrained, done],
        &[
            Seen::StdoutEof,
            Seen::Exited { waited: false },
            Seen::StderrDrained,
            done,
        ],
    ];
    for seen in incomplete {
        let observation = observed(&attempt, seen);
        assert!(observation.terminal_evidence().is_none(), "{seen:?}");
        assert!(observation.no_effect_evidence().is_none(), "{seen:?}");
    }
    let broken = observed(&attempt, &[&COMPLETE[..], &[Seen::Broken, error]].concat());
    assert!(broken.terminal_evidence().is_none() && broken.no_effect_evidence().is_none());
    let done_then_error = observed(&attempt, &[&COMPLETE[..], &[done, error]].concat());
    assert!(done_then_error.terminal_evidence().is_none());
    let finished = observed(&attempt, &[&COMPLETE[..], &[done]].concat());
    assert_eq!(
        finished.terminal_evidence().map(|e| e.kind()),
        Some(TerminalKind::Done)
    );
    assert!(
        finished.no_effect_evidence().is_none(),
        "a finished turn is an effect"
    );
    for activity in [
        Activity::Output,
        Activity::Tool,
        Activity::Unclassified,
        Activity::InjectedInput,
    ] {
        let active = observed(
            &attempt,
            &[&COMPLETE[..], &[Seen::Activity(activity)]].concat(),
        );
        assert!(active.no_effect_evidence().is_none(), "{activity:?}");
        assert_eq!(active.hold_reason(), "activity after start");
    }
    let quiet = observed(&attempt, &[&COMPLETE[..], &[error]].concat());
    assert!(quiet.no_effect_evidence().is_some());
    assert_eq!(
        quiet.terminal_evidence().map(|e| e.kind()),
        Some(TerminalKind::Error)
    );
    finish(fixture, pool).await;
}

#[tokio::test(flavor = "current_thread")]
async fn classification_commits_only_for_its_exact_attempt_pg() {
    let (fixture, pool) = setup().await;
    let first = start(&pool, &input("first", "chan", &["c1"])).await;
    let other = start(&pool, &input("other", "chan", &["c2"])).await;
    let done = [&COMPLETE[..], &[Seen::Terminal(TerminalKind::Done)]].concat();
    let other_terminal = observed(&other, &done).terminal_evidence().unwrap();
    assert!(matches!(
        write::classify_normal(&pool, &first, &other_terminal).await,
        Err(WriteError::CasMiss)
    ));
    assert_eq!(
        receipt_row(&pool, first.receipt_id()).await.0,
        "started_unclassified"
    );
    let terminal = observed(&first, &done).terminal_evidence().unwrap();
    write::classify_normal(&pool, &first, &terminal)
        .await
        .expect("exact classification");
    assert_eq!(
        receipt_row(&pool, first.receipt_id()).await.0,
        "classified_normal"
    );
    assert!(matches!(
        write::classify_normal(&pool, &first, &terminal).await,
        Err(WriteError::CasMiss)
    ));
    let hold = serde_json::json!({ "partial_body": "kept" });
    assert!(matches!(
        write::withhold(&pool, &first, "late", &hold).await,
        Err(WriteError::CasMiss)
    ));
    write::withhold(&pool, &other, "activity after start", &hold)
        .await
        .expect("hold the other attempt");
    let (disposition, _, _, preserved) = receipt_row(&pool, other.receipt_id()).await;
    assert_eq!(disposition, "withheld");
    assert_eq!(preserved["partial_body"], "kept");
    assert_eq!(preserved["projection"]["nonce"], other.nonce());
    pool.close().await;
    assert!(
        matches!(
            write::withhold(&pool, &other, "late", &hold).await,
            Err(WriteError::Storage(sqlx::Error::PoolClosed))
        ),
        "a storage failure is never reported as a committed hold"
    );
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn begin_commit_failure_reports_ack_unknown_not_a_start_pg() {
    let (fixture, pool) = setup().await;
    sqlx::raw_sql(
        "CREATE FUNCTION lose_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'commit acknowledgement lost'; END $$;
         CREATE CONSTRAINT TRIGGER lose_start_commit AFTER UPDATE ON intake_outbox
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
             WHEN (NEW.replay_disposition = 'started_unclassified')
             EXECUTE FUNCTION lose_commit();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (id, nonce) = register(&pool, &input("lost-ack", "chan", &["l1"])).await;
    let binding = serde_json::json!({});
    let from = BeginFrom::Registered { nonce: &nonce };
    let outcome = write::begin(&pool, id, from, &effect(&binding), "incarnation-a").await;
    assert!(
        matches!(&outcome, Err(WriteError::AckUnknown(error))
            if error.to_string().contains("commit acknowledgement lost")),
        "a lost commit acknowledgement is unknown, never a start or a clean failure: {outcome:?}"
    );
    finish(fixture, pool).await;
}
