use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::exact_episode::{
    Authority, EpisodeEvidence, EpisodeMetadata, ExactEpisodePin, SubmissionBasis,
};
use crate::services::tui_o::exact_pg::{record_episode_evidence, resolve_in_tx};
use crate::services::tui_o::exact_submission::*;
use uuid::Uuid;

fn pin() -> ExactEpisodePin {
    let record = crate::services::tui_o::exact_episode::tests::fixture().remove(0);
    let EpisodeEvidence::Pin(mut pin) = record.evidence else {
        panic!("fixture pin");
    };
    pin.episode = Uuid::new_v4();
    pin.source = None;
    pin
}

async fn persist_pin(pool: &sqlx::PgPool, pin: &ExactEpisodePin) {
    crate::services::discord::health::test_support::install_runtime_ports_for_tests();
    record_episode_evidence(
        true,
        pool,
        &EpisodeMetadata::new(
            pin.episode,
            Uuid::new_v4(),
            EpisodeEvidence::Pin(pin.clone()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_submission_pg_executor_classifies_no_attempt_and_typed_refusal() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for scenario in 0..4 {
        let pin = pin();
        persist_pin(&pool, &pin).await;
        let p = pool.clone();
        let copy = pin.clone();
        tokio::task::spawn_blocking(move || {
            let _installed = install(p, copy);
            assert!(crate::services::provider::herdr_provider_terminal_only(None).is_some());
            let result = dispatch(|| {
                if scenario > 0 {
                    begin_input_for_test()?;
                    observe_untouched_for_test(scenario == 1);
                }
                Err::<(), _>("executor exit after gate or launch".into())
            });
            assert_eq!(result, Err("executor exit after gate or launch".into()));
        })
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let result = resolve_in_tx(&mut connection, pin.episode).await.unwrap();
        assert_eq!(
            result.authority(),
            if scenario < 2 {
                Authority::Policy
            } else {
                Authority::Pending
            }
        );
        let closures: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events WHERE canonical_payload->>'episode'=$1 AND canonical_payload->'evidence'->>'type'='SubmissionClosed'")
            .bind(pin.episode.to_string()).fetch_one(&mut *connection).await.unwrap();
        assert_eq!(closures, i64::from(scenario < 2));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_submission_pg_reopen_process_resolves_policy_without_source() {
    if let Ok(url) = std::env::var("C2A_SUBMISSION_READER") {
        let pool = crate::db::postgres::connect_test_pool(&url, "submission child")
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let episode = std::env::var("C2A_SUBMISSION_EPISODE")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            resolve_in_tx(&mut connection, episode)
                .await
                .unwrap()
                .authority(),
            Authority::Policy
        );
        assert!(
            logical_key_for_test().is_none(),
            "no process-local capability survives"
        );
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let pin = pin();
    persist_pin(&pool, &pin).await;
    let p = pool.clone();
    let copy = pin.clone();
    tokio::task::spawn_blocking(move || {
        let _installed = install(p, copy);
        assert_eq!(
            dispatch(|| Err::<(), _>("launch refused".into())),
            Err("launch refused".into())
        );
    })
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "services::discord::org_writer::krx_discord_root_tests::tui_o_exact_submission::exact_submission_pg_reopen_process_resolves_policy_without_source", "--nocapture"])
        .env("C2A_SUBMISSION_READER", &db.database_url)
        .env("C2A_SUBMISSION_EPISODE", pin.episode.to_string())
        .env("AGENTDESK_ROOT_DIR", root.path()).output().unwrap();
    assert!(
        child.status.success(),
        "{} {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        0,
        "PG-only policy replay writes nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_submission_pg_closed_admission_rejects_late_attempt() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let pin = pin();
    persist_pin(&pool, &pin).await;
    let closure = EpisodeMetadata::new(
        pin.episode,
        Uuid::new_v4(),
        EpisodeEvidence::SubmissionClosed {
            generation: pin.born_generation,
            basis: SubmissionBasis::NoAttempt,
            policy_version: pin.context.policy_version,
        },
    );
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    *COMMIT_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CommitBarrier {
        episode: pin.episode,
        entered,
        release: released,
    });
    let p = pool.clone();
    let copy = closure.clone();
    let close = tokio::spawn(async move { record_episode_evidence(true, &p, &copy).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("closure reached commit barrier")
        .unwrap();
    let attempt = EpisodeMetadata::new(
        pin.episode,
        Uuid::new_v4(),
        EpisodeEvidence::InputAttemptBegun {
            nonce: Uuid::new_v4(),
        },
    );
    let p = pool.clone();
    let mut late = tokio::spawn(async move { record_episode_evidence(true, &p, &attempt).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut late)
            .await
            .is_err(),
        "late attempt must wait for closure transaction"
    );
    release.send(()).unwrap();
    close.await.unwrap().unwrap();
    assert!(
        late.await.unwrap().is_err(),
        "closed admission rejects racing attempt"
    );
    // Exact retry remains acknowledged, not a second closure.
    record_episode_evidence(true, &pool, &closure)
        .await
        .unwrap();
    let p = pool.clone();
    let copy = pin.clone();
    tokio::task::spawn_blocking(move || {
        let _installed = install(p, copy);
        assert!(begin_input_for_test().is_err());
        assert_eq!(
            dispatch(|| Err::<(), _>("input blocked".into())),
            Err("input blocked".into())
        );
    })
    .await
    .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    assert_eq!(
        resolve_in_tx(&mut connection, pin.episode)
            .await
            .unwrap()
            .authority(),
        Authority::Policy
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_submission_pg_attempt_before_closure_never_becomes_no_attempt() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let pin = pin();
    persist_pin(&pool, &pin).await;
    let attempt = EpisodeMetadata::new(
        pin.episode,
        Uuid::new_v4(),
        EpisodeEvidence::InputAttemptBegun {
            nonce: Uuid::new_v4(),
        },
    );
    record_episode_evidence(true, &pool, &attempt)
        .await
        .unwrap();
    let closure = EpisodeMetadata::new(
        pin.episode,
        Uuid::new_v4(),
        EpisodeEvidence::SubmissionClosed {
            generation: pin.born_generation,
            basis: SubmissionBasis::NoAttempt,
            policy_version: pin.context.policy_version,
        },
    );
    assert!(
        record_episode_evidence(true, &pool, &closure)
            .await
            .is_err()
    );
    let mut connection = pool.acquire().await.unwrap();
    assert_eq!(
        resolve_in_tx(&mut connection, pin.episode)
            .await
            .unwrap()
            .authority(),
        Authority::Pending
    );
}
