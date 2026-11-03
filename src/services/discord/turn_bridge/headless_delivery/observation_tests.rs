use super::*;
use crate::server::outbox_worker::{drain_message_outbox_once, origin_fixture::Discord};
use crate::services::cluster::intake_router_hook::owner_record::output;
use crate::services::discord::outbound::{census_scope, raw_output::tests as recording};
use crate::services::message_outbox;
use serde_json::{Value, json};
use std::path::Path;

fn records(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "target")
        .collect()
}

async fn enqueue(
    pool: &sqlx::PgPool,
    origin: &output::Origin,
    key: &str,
    tag: Option<Value>,
) -> i64 {
    let artifact = origin.seal("stored answer");
    let outcome = census_scope(
        tag,
        message_outbox::enqueue_outbox_pg_with_exact_origin(
            pool,
            OutboxMessage {
                target: "channel:72280700",
                content: "stored answer",
                bot: "notify",
                source: "headless_turn",
                reason_code: Some("census.delivery"),
                session_key: None,
            },
            key,
            None,
            None,
            Some(&artifact),
        ),
    )
    .await
    .unwrap();
    match outcome {
        OutboxEnqueueOutcome::Enqueued { id } => id,
        other => panic!("{other:?}"),
    }
}

fn tag(mode: &str, host: &str) -> Value {
    json!({"site":"W17","binding_mode":mode,"output_class":"LegacyBody",
        "actual_runtime":"fixture-runtime","session_temperature":"unknown",
        "source_incarnation":{"execution_digest":"saved"},"producer_host":host,
        "input_origin":"unknown","provider":null,"body_owner":"Legacy","reason":"NO_CLAIM"})
}

#[test]
fn registered_agent_labels_do_not_certify_absent_body_metadata() {
    if recording::isolated(
        concat!(
            module_path!(),
            "::registered_agent_labels_do_not_certify_absent_body_metadata"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let mut config = crate::config::Config::default();
    for id in ["census-agent", "system"] {
        let input = crate::services::discord::agentdesk_config::AgentSetupConfigInput {
            agent_id: id.into(),
            provider: "claude".into(),
            channel_id: "72280700".into(),
            prompt_file: "fixture.md".into(),
            workspace: "/tmp".into(),
        };
        crate::services::discord::agentdesk_config::ensure_agent_setup_config(&mut config, &input);
    }
    crate::config_live_reload::install(config);
    for id in ["census-agent", "system"] {
        assert!(message_outbox::validate_outbox_source(id).is_ok());
        let tag = message_outbox::body_census::delivery_tag(id, None);
        assert_eq!(tag["output_class"], "UnknownBody", "{id}");
        assert_eq!(tag["binding_mode"], "UNKNOWN", "{id}");
    }
}

#[tokio::test]
async fn old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg() {
    if recording::isolated(
        concat!(
            module_path!(),
            "::old_outbox_rows_and_bad_metadata_keep_wire_and_unknown_source_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let (_db, pool, origin) = output::tests::fixture("72280700").await;
    let log = recording::journal().await;
    let mock = Discord::new().await;
    let registry = mock.registry().await;
    for (index, kind) in ["null", "schema", "decode", "partial"].iter().enumerate() {
        let id = enqueue(&pool, &origin, &format!("old-{kind}"), None).await;
        if *kind == "schema" {
            sqlx::query("UPDATE message_outbox SET body_census=$2 WHERE id=$1")
                .bind(id)
                .bind(json!({"schema_version":99}))
                .execute(&pool)
                .await
                .unwrap();
        } else if *kind == "partial" {
            sqlx::query("UPDATE message_outbox SET body_census=$2 WHERE id=$1")
                .bind(id)
                .bind(json!({"schema_version":1,
                    "producer":{"boot_id":uuid::Uuid::new_v4(),"host":null},
                    "tag":{"output_class":"LegacyBody","binding_mode":"NON_TUI"}}))
                .execute(&pool)
                .await
                .unwrap();
        } else if *kind == "decode" {
            sqlx::query("UPDATE message_outbox SET body_census='1e1000'::jsonb WHERE id=$1")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(
            drain_message_outbox_once(
                &Arc::new(pool.clone()),
                &registry,
                &format!("old-worker-{index}")
            )
            .await,
            1
        );
        assert_eq!(mock.posts().len(), index + 1);
        recording::flushed().await;
        let rows = records(&log);
        assert_eq!(rows.len(), (index + 1) * 2);
        assert_eq!(
            rows.last().unwrap()["census"]["output_class"],
            "UnknownBody"
        );
        assert_eq!(rows.last().unwrap()["census"]["binding_mode"], "UNKNOWN");
        assert_eq!(rows.last().unwrap()["outcome"], "confirmed");
    }
    let mut tx = pool.begin().await.unwrap();
    message_outbox::enqueue_outbox_pg_on_tx(
        &mut tx,
        OutboxMessage {
            target: "channel:72280700",
            content: "operational notice",
            bot: "notify",
            source: "system",
            reason_code: Some("census.nonbody"),
            session_key: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        drain_message_outbox_once(&Arc::new(pool), &registry, "nonbody-worker").await,
        1
    );
    recording::flushed().await;
    assert_eq!(mock.posts().len(), 5);
    assert_eq!(
        records(&log).len(),
        8,
        "registered nonbody does not become a body sample"
    );
}

#[tokio::test]
async fn metadata_projection_failure_keeps_original_authority_pg() {
    if recording::isolated(
        concat!(
            module_path!(),
            "::metadata_projection_failure_keeps_original_authority_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let (_db, pool, origin) = output::tests::fixture("72280700").await;
    let log = recording::journal().await;
    enqueue(
        &pool,
        &origin,
        "projection-failed",
        Some(tag("TUI", "original-node")),
    )
    .await;
    sqlx::query("CREATE FUNCTION to_jsonb(message_outbox) RETURNS jsonb LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture metadata projection unavailable'; END $$")
        .execute(&pool).await.unwrap();
    assert!(
        sqlx::query("SELECT to_jsonb(m) FROM message_outbox m")
            .execute(&pool)
            .await
            .is_err()
    );
    let mock = Discord::new().await;
    assert_eq!(
        drain_message_outbox_once(&Arc::new(pool), &mock.registry().await, "projection-worker")
            .await,
        1
    );
    assert_eq!(
        mock.posts().len(),
        1,
        "optional metadata cannot withhold an authorized send"
    );
    recording::flushed().await;
    let rows = records(&log);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["outcome"], "confirmed");
    assert_eq!(rows[1]["census"]["output_class"], "UnknownBody");
    assert_eq!(rows[1]["census"]["binding_mode"], "UNKNOWN");
}

#[tokio::test]
async fn census_write_failure_and_dedupe_preserve_enqueue_authority_pg() {
    if recording::isolated(
        concat!(
            module_path!(),
            "::census_write_failure_and_dedupe_preserve_enqueue_authority_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let (_db, pool, origin) = output::tests::fixture("72280700").await;
    let log = recording::journal().await;
    let id = enqueue(
        &pool,
        &origin,
        "original",
        Some(tag("TUI", "original-node")),
    )
    .await;
    let read = || {
        sqlx::query_scalar::<_, Option<Value>>("SELECT body_census FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
    };
    let saved = read().await.unwrap();
    assert_eq!(
        saved.as_ref().unwrap()["tag"]["producer_host"],
        "original-node"
    );
    let (first, second) = tokio::join!(
        enqueue(
            &pool,
            &origin,
            "original",
            Some(tag("NON_TUI", "replacement-a"))
        ),
        enqueue(
            &pool,
            &origin,
            "original",
            Some(tag("NON_TUI", "replacement-b"))
        ),
    );
    assert_eq!((first, second), (id, id));
    assert_eq!(
        read().await.unwrap(),
        saved,
        "a duplicate must retain the first producer"
    );
    let old = enqueue(&pool, &origin, "old-null", None).await;
    assert_eq!(
        enqueue(&pool, &origin, "old-null", Some(tag("NON_TUI", "new-node"))).await,
        old
    );
    let old_tag: Option<Value> =
        sqlx::query_scalar("SELECT body_census FROM message_outbox WHERE id=$1")
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        old_tag.is_none(),
        "duplicate enqueue is not a headless backfill"
    );
    sqlx::query("CREATE FUNCTION refuse_body_census() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refuses observation'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_body_census BEFORE UPDATE OF body_census ON message_outbox FOR EACH ROW EXECUTE FUNCTION refuse_body_census()")
        .execute(&pool).await.unwrap();
    let failed = enqueue(
        &pool,
        &origin,
        "write-failed",
        Some(tag("TUI", "original-node")),
    )
    .await;
    let (stored, artifact): (Option<Value>, Option<Value>) =
        sqlx::query_as("SELECT body_census,output_origin FROM message_outbox WHERE id=$1")
            .bind(failed)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(stored.is_none());
    assert_eq!(
        artifact,
        Some(serde_json::to_value(origin.seal("stored answer")).unwrap())
    );
    let mock = Discord::new().await;
    assert_eq!(
        drain_message_outbox_once(
            &Arc::new(pool.clone()),
            &mock.registry().await,
            "write-failed-worker"
        )
        .await,
        3
    );
    assert_eq!(
        mock.posts().len(),
        3,
        "observation failure must not prevent original authorized sends"
    );
    recording::flushed().await;
    let rows = records(&log);
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[1]["census"]["binding_mode"], "TUI");
    assert_eq!(rows[3]["census"]["binding_mode"], "UNKNOWN");
    assert_eq!(rows[5]["census"]["binding_mode"], "UNKNOWN");
    assert!(
        rows.iter()
            .filter(|r| r["phase"] == "result")
            .all(|r| r["outcome"] == "confirmed")
    );
}

#[tokio::test]
async fn outbox_census_survives_a_different_process_worker_pg() {
    if let Ok(database) = std::env::var("ADK_CENSUS_CONSUMER_DATABASE") {
        let _live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let mut config = crate::config::Config::default();
        config.cluster.instance_id = Some("consumer-process".into());
        config.data.dir =
            std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap()).join("data");
        crate::config_live_reload::install(config);
        let pool = Arc::new(sqlx::PgPool::connect(&database).await.unwrap());
        let log = recording::journal().await;
        let mock = Discord::new().await;
        let registry = mock.registry().await;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(log.parent().unwrap().join("writer.lock"))
            .unwrap();
        lock.lock().unwrap();
        let delivery = drain_message_outbox_once(&pool, &registry, "new-process-worker");
        tokio::pin!(delivery);
        let before =
            tokio::time::timeout(std::time::Duration::from_millis(1500), &mut delivery).await;
        let completed = before.is_ok();
        let wire = mock.posts().len();
        drop(lock);
        let claimed = match before {
            Ok(claimed) => claimed,
            Err(_) => delivery.await,
        };
        assert_eq!(claimed, 1);
        assert!(
            completed && wire == 1,
            "restored outbox observation cannot wait for journal I/O"
        );
        recording::flushed().await;
        assert_eq!(mock.posts().len(), 1);
        let rows = records(&log);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["outcome"], "confirmed");
        assert_eq!(rows[1]["census"]["producer_host"], "original-node");
        let original = std::env::var("ADK_CENSUS_ORIGINAL_BOOT").unwrap();
        assert_eq!(rows[1]["census"]["producer_boot_id"], original);
        assert_ne!(crate::services::process_boot::initialize().id, original);
        assert_eq!(
            rows[1]["census"]["producer_process_host"],
            "producer-process"
        );
        assert_eq!(
            crate::services::legacy_body_census::health_json()["host"],
            "consumer-process"
        );
        assert_eq!(rows[1]["census"]["binding_mode"], "TUI");
        return;
    }
    if recording::isolated(
        concat!(
            module_path!(),
            "::outbox_census_survives_a_different_process_worker_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    use sqlx::ConnectOptions;
    let (_db, pool, origin) = output::tests::fixture("72280700").await;
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some("producer-process".into());
    config.data.dir =
        std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap()).join("data");
    crate::config_live_reload::install(config);
    let _log = recording::journal().await;
    enqueue(&pool, &origin, "restart", Some(tag("TUI", "original-node"))).await;
    let root = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            concat!(
                module_path!(),
                "::outbox_census_survives_a_different_process_worker_pg"
            )
            .trim_start_matches("agentdesk::"),
            "--nocapture",
        ])
        .env(
            "ADK_CENSUS_CONSUMER_DATABASE",
            pool.connect_options().to_url_lossy().as_str(),
        )
        .env(
            "ADK_CENSUS_ORIGINAL_BOOT",
            &crate::services::process_boot::initialize().id,
        )
        .env("AGENTDESK_ROOT_DIR", root.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM message_outbox WHERE dedupe_key='restart'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "sent");
}

#[tokio::test(flavor = "current_thread")]
async fn w17_direct_fallback_keeps_runtime_mode_while_journal_is_blocked() {
    if recording::isolated(
        concat!(
            module_path!(),
            "::w17_direct_fallback_keeps_runtime_mode_while_journal_is_blocked"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    use crate::services::agent_protocol::RuntimeHandoffKind;
    let log = recording::journal().await;
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    for (kind, expected) in [
        (Some(RuntimeHandoffKind::ClaudeTui), "TUI"),
        (Some(RuntimeHandoffKind::ProcessBackend), "NON_TUI"),
        (None, "UNKNOWN"),
    ] {
        let mock = recording::Mock::start(vec![recording::message(9401)]).await;
        let _http = crate::services::discord::shared_state::test_rest::install(Arc::new(
            recording::client(&mock),
        ));
        let mut state = test_inflight_state();
        state.runtime_kind = kind;
        let provider = ProviderKind::Claude;
        let arguments = assemble_headless_delivery_arguments(
            &state,
            HeadlessDeliveryInputs {
                shared: &shared,
                channel_id: ChannelId::new(recording::CHANNEL),
                owning_user_msg_id: Some(MessageId::new(8)),
                session_key: None,
                provider: &provider,
                content: "direct answer",
                cancel_token: None,
            },
        );
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(log.parent().unwrap().join("writer.lock"))
            .unwrap();
        lock.lock().unwrap();
        let delivery = enqueue_claimed_headless_delivery(None, arguments);
        tokio::pin!(delivery);
        let before =
            tokio::time::timeout(std::time::Duration::from_millis(1500), &mut delivery).await;
        let completed = before.is_ok();
        let wire = mock.requests().len();
        drop(lock);
        let outcome = match before {
            Ok(outcome) => outcome,
            Err(_) => delivery.await,
        };
        assert!(matches!(outcome, HeadlessDeliveryOutcome::Delivered));
        assert!(
            completed && wire == 1,
            "journal observation must not delay W17 wire"
        );
        recording::flushed().await;
        let rows = records(&log);
        assert_eq!(rows.last().unwrap()["census"]["binding_mode"], expected);
        assert_eq!(rows.last().unwrap()["census"]["site"], "W17");
        assert_eq!(rows.last().unwrap()["census"]["reason"], "NO_CLAIM");
    }
    assert_eq!(records(&log).len(), 6);
}
