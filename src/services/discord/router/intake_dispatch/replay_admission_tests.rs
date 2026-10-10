use super::replay_admission::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::WriteError;
use crate::services::discord::replay_policy::live::{RawObservation, Seen};
use crate::services::discord::replay_policy::permit::{EffectTarget, PreparedRetryStart};
use crate::services::discord::replay_policy::{ReplayDecision, decide_stale};
use sqlx::PgPool;

fn request(key: &str, sources: &[&str]) -> CanonicalInput {
    CanonicalInput {
        request_key: key.into(),
        provider: "claude".into(),
        channel: "chan".into(),
        sources: sources.iter().map(|source| source.to_string()).collect(),
        original_text: format!("original {key}"),
        owner_id: "user".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([{ "id": "attachment-1" }]),
        reply_context: Some("reply".into()),
        provenance: "new_input",
    }
}

fn projection(session_key: Option<&str>) -> PreparedEffectProjection {
    PreparedEffectProjection {
        target: EffectTarget::ProviderStart {
            provider: "claude".into(),
            channel: "chan".into(),
        },
        input_hash: "prepared-input".into(),
        binding: serde_json::json!({ "owner": "owner-a" }),
        session_key: session_key.map(str::to_string),
    }
}

fn active(pool: &PgPool) -> ReplayAdmissionContext<'_> {
    ReplayAdmissionContext {
        producers: ProducerMode::Active,
        ..ReplayAdmissionContext::production(pool, "node-a", "incarnation-a")
    }
}

fn admitted(preflight: ReplayPreflight) -> ReplayCandidate {
    match preflight {
        ReplayPreflight::Admit(candidate) => candidate,
        other => panic!("expected admission, got {other:?}"),
    }
}

async fn started(ctx: &ReplayAdmissionContext<'_>, input: &CanonicalInput) -> i64 {
    let candidate = admitted(preflight_source(ctx, input).await);
    let permit = prepare_effect_start(ctx, input, candidate, projection(None), None)
        .await
        .expect("fresh start permit");
    permit.attempt().receipt_id()
}

async fn replay_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn setup() -> (TestPostgresDb, PgPool) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    (fixture, pool)
}

#[tokio::test(flavor = "current_thread")]
async fn preflight_sends_every_blocking_state_to_protected_consumption_pg() {
    let (fixture, pool) = setup().await;
    let ctx = active(&pool);
    let held = started(&ctx, &request("held", &["h1", "h2"])).await;
    for (state, extra) in [
        ("startup_failed_no_effect", ""),
        ("withheld", ", replay_hold_reason = 'held'"),
    ] {
        let id = started(&ctx, &request(state, &[state])).await;
        let sql =
            format!("UPDATE intake_outbox SET replay_disposition = '{state}'{extra} WHERE id = $1");
        sqlx::query(&sql).bind(id).execute(&pool).await.unwrap();
        let ReplayPreflight::ConsumeProtected(set) =
            preflight_source(&ctx, &request("again", &[state])).await
        else {
            panic!("{state} must be consumed as protected");
        };
        assert_eq!(
            set.receipts.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![id]
        );
    }
    for sources in [&["h2"][..], &["h1", "h2"]] {
        let ReplayPreflight::ConsumeProtected(set) =
            preflight_source(&ctx, &request("subset", sources)).await
        else {
            panic!("an absorbed source subset must be consumed as protected");
        };
        assert_eq!(set.receipts[0].id, held);
        assert_eq!(set.receipts[0].original_text, "original held");
    }
    let mixed = preflight_source(&ctx, &request("mixed", &["h1", "new"])).await;
    assert!(
        matches!(mixed, ReplayPreflight::Defer(ReplayDefer::MixedSources)),
        "{mixed:?}"
    );
    let blank = preflight_source(&ctx, &request("", &["x"])).await;
    assert!(matches!(
        blank,
        ReplayPreflight::Defer(ReplayDefer::IdentityUnknown)
    ));
    let none = preflight_source(&ctx, &request("no-sources", &[])).await;
    assert!(matches!(
        none,
        ReplayPreflight::Defer(ReplayDefer::IdentityUnknown)
    ));

    sqlx::raw_sql(
        "ALTER TABLE intake_outbox DROP CONSTRAINT intake_outbox_replay_disposition_check;
         ALTER TABLE intake_outbox DISABLE TRIGGER USER;
         UPDATE intake_outbox SET replay_disposition = 'future_state'
          WHERE replay_request_key = 'withheld';
         ALTER TABLE intake_outbox ENABLE TRIGGER USER;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let unknown = preflight_source(&ctx, &request("unknown", &["withheld"])).await;
    assert!(matches!(
        unknown,
        ReplayPreflight::Defer(ReplayDefer::UnknownDisposition)
    ));
    pool.close().await;
    let failed = preflight_source(&ctx, &request("fresh", &["f1"])).await;
    assert!(
        matches!(failed, ReplayPreflight::Defer(ReplayDefer::LookupFailed)),
        "{failed:?}"
    );
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_commits_the_receipt_before_the_permit_and_never_reissues_pg() {
    let (fixture, pool) = setup().await;
    let ctx = active(&pool);
    let input = request("once", &["o1", "o2"]);
    let first = admitted(preflight_source(&ctx, &input).await);
    let second = admitted(preflight_source(&ctx, &input).await);
    let permit = prepare_effect_start(&ctx, &input, first, projection(None), None)
        .await
        .expect("first preparation starts");
    let id = permit.attempt().receipt_id();
    let (disposition, text, sources, attachments, preserved): (
        String,
        String,
        Vec<String>,
        serde_json::Value,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT replay_disposition, user_text, replay_source_message_ids, attachment_refs,
                replay_preserved FROM intake_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        disposition, "started_unclassified",
        "committed before the permit exists"
    );
    assert_eq!(
        (text.as_str(), sources),
        ("original once", input.sources.clone())
    );
    assert_eq!(attachments, input.attachments);
    assert_eq!(preserved["projection"]["input_hash"], "prepared-input");
    assert!(matches!(
        prepare_effect_start(&ctx, &input, second, projection(None), None).await,
        Err(StartRefusal::Protected(protected)) if protected == id
    ));
    assert!(matches!(
        preflight_source(&ctx, &input).await,
        ReplayPreflight::ConsumeProtected(_)
    ));
    sqlx::query("UPDATE intake_outbox SET replay_disposition = 'classified_normal' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let again = admitted(preflight_source(&ctx, &input).await);
    assert!(
        matches!(
            prepare_effect_start(&ctx, &input, again, projection(None), None).await,
            Err(StartRefusal::AlreadyClassified(classified)) if classified == id
        ),
        "a classified request goes back to its ordinary policy, not to a new start"
    );

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
    let lost = request("lost-ack", &["l1"]);
    let candidate = admitted(preflight_source(&ctx, &lost).await);
    let late = admitted(preflight_source(&ctx, &lost).await);
    let outcome = prepare_effect_start(&ctx, &lost, candidate, projection(None), None).await;
    assert!(
        matches!(&outcome, Err(StartRefusal::Write(WriteError::AckUnknown(error)))
            if error.to_string().contains("commit acknowledgement lost")),
        "{outcome:?}"
    );
    sqlx::raw_sql(
        "DROP TRIGGER lose_start_commit ON intake_outbox;
         UPDATE intake_outbox SET replay_disposition = 'started_unclassified',
                replay_owner_incarnation = 'incarnation-a',
                replay_preserved = replay_preserved || jsonb_build_object('projection',
                    jsonb_build_object('nonce', replay_episode_nonce))
          WHERE replay_request_key = 'lost-ack';",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        matches!(
            prepare_effect_start(&ctx, &lost, late, projection(None), None).await,
            Err(StartRefusal::Protected(_))
        ),
        "a start whose commit did land is never started again"
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn dormant_production_context_creates_no_receipt_even_with_producers_env_on_pg() {
    let env = crate::config::TestEnvVarGuard::set_path(
        "ADK_REPLAY_HOLD_PRODUCERS",
        std::path::Path::new("on"),
    );
    let (fixture, pool) = setup().await;
    let ctx = ReplayAdmissionContext::production(&pool, "node-a", "incarnation-a");
    drop(env);
    let held = started(&active(&pool), &request("held", &["h1"])).await;
    let registered = request("registered", &["r1"]);
    crate::db::replay_disposition::write::register_or_reuse(&pool, &registered, "node-a")
        .await
        .unwrap();
    let before = replay_rows(&pool).await;
    assert_eq!(ctx.producers, ProducerMode::Dormant);
    let fresh = request("fresh", &["f1"]);
    let preflight = preflight_source(&ctx, &fresh).await;
    assert!(
        matches!(preflight, ReplayPreflight::LegacyUnprotected),
        "{preflight:?}"
    );
    let forged = admitted(preflight_source(&active(&pool), &fresh).await);
    assert!(matches!(
        prepare_effect_start(&ctx, &fresh, forged, projection(None), None).await,
        Err(StartRefusal::ProducerDormant)
    ));
    assert_eq!(
        replay_rows(&pool).await,
        before,
        "dormant production writes no receipt"
    );
    let ReplayPreflight::ConsumeProtected(set) =
        preflight_source(&ctx, &request("again", &["h1"])).await
    else {
        panic!("dormant still consumes an existing hold");
    };
    assert_eq!(set.receipts[0].id, held);
    let waiting = preflight_source(&ctx, &registered).await;
    assert!(
        matches!(
            waiting,
            ReplayPreflight::Defer(ReplayDefer::ProducerDormant)
        ),
        "{waiting:?}"
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn exact_retry_reenters_only_through_its_own_continuation_pg() {
    let (fixture, pool) = setup().await;
    let ctx = active(&pool);
    let input = request("retry", &["t1"]);
    let candidate = admitted(preflight_source(&ctx, &input).await);
    let permit = prepare_effect_start(&ctx, &input, candidate, projection(None), None)
        .await
        .unwrap();
    let mut observation = None;
    let attempt = permit
        .consume(&projection(None).target, "prepared-input", |attempt| {
            let mut seen = RawObservation::for_attempt(&attempt);
            for item in [
                Seen::StdoutEof,
                Seen::Exited { waited: true },
                Seen::StderrDrained,
            ] {
                seen.record(item);
            }
            observation = Some(seen);
            attempt
        })
        .unwrap();
    let (id, previous) = (attempt.receipt_id(), attempt.nonce().to_string());
    sqlx::query(
        "INSERT INTO sessions (session_key, channel_id, provider, status, current_replay_receipt_id,
                               replay_episode_nonce, claude_session_id)
         VALUES ('retry-session', 'chan', 'claude', 'turn_active', $1, $2, 'stale-resume')",
    )
    .bind(id)
    .bind(&previous)
    .execute(&pool)
    .await
    .unwrap();
    let ReplayDecision::AllowStartupRetry(retry) =
        decide_stale(&pool, Some(attempt), &observation.unwrap()).await
    else {
        panic!("no-effect attempt allows one retry");
    };

    started(&ctx, &request("other", &["x1"])).await;
    let ReplayPreflight::ConsumeProtected(foreign) =
        preflight_source(&ctx, &request("x", &["x1"])).await
    else {
        panic!("another started request is protected");
    };
    assert!(
        foreign.resume(&retry).is_err(),
        "a continuation never resumes another receipt"
    );
    let ReplayPreflight::ConsumeProtected(set) = preflight_source(&ctx, &input).await else {
        panic!("the retried request is protected until its continuation re-enters");
    };
    let candidate = set.resume(&retry).expect("own continuation resumes");
    assert!(
        matches!(
            prepare_effect_start(
                &ctx,
                &input,
                candidate,
                projection(Some("retry-session")),
                None
            )
            .await,
            Err(StartRefusal::CandidateMismatch)
        ),
        "a resumed candidate without its continuation is refused"
    );
    let ReplayPreflight::ConsumeProtected(set) = preflight_source(&ctx, &input).await else {
        panic!("still protected after a refused preparation");
    };
    let candidate = set.resume(&retry).unwrap();
    let permit = prepare_effect_start(
        &ctx,
        &input,
        candidate,
        projection(Some("retry-session")),
        Some(retry),
    )
    .await
    .expect("exact retry permit");
    assert_eq!(permit.attempt().retry_of(), Some(previous.as_str()));
    let next = permit.attempt().nonce().to_string();
    let session: (Option<String>, String) = sqlx::query_as(
        "SELECT claude_session_id, replay_episode_nonce FROM sessions WHERE session_key = 'retry-session'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        session,
        (None, next.clone()),
        "the stale resume id never reaches the retry"
    );
    let prepared = PreparedRetryStart::new(("fresh-args", next), permit).expect("retry permit");
    let order = std::cell::RefCell::new(Vec::new());
    let launched = prepared.launch(
        |values| order.borrow_mut().push(format!("reset {}", values.1)),
        |values, permit| {
            order.borrow_mut().push("start".to_string());
            permit.consume(&projection(None).target, "prepared-input", |attempt| {
                (values, attempt.nonce().to_string())
            })
        },
    );
    let ((args, nonce), used) = launched.unwrap();
    assert_eq!((args, nonce.as_str()), ("fresh-args", used.as_str()));
    assert_eq!(
        order.into_inner(),
        vec![format!("reset {used}"), "start".to_string()]
    );
    let ReplayPreflight::ConsumeProtected(set) = preflight_source(&ctx, &input).await else {
        panic!("a started retry stays protected");
    };
    assert_eq!(
        set.receipts[0].episode_nonce.as_deref(),
        Some(used.as_str())
    );
    pool.close().await;
    fixture.drop().await;
}
