//! A live admission followed by the owner's real idle start must reload changed policy.
use super::*;
use crate::services::discord::queue_io::input_admission::Admission;
use crate::services::discord::queue_io::input_ingress;
use crate::services::discord::router::herdr_consent::admission::tests::install_final_local_probe;
use crate::services::discord::shared_state::test_rest;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;
use serde_json::json;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

const CHANNEL: u64 = 82_073_101;
const MESSAGE: u64 = 1_569_308_719_030_820_731;

#[derive(Clone, Copy)]
enum Change {
    Requirements,
    DefaultNode,
    Unchanged,
}

async fn reentry(change: Change) {
    let env = ScopedIntakeTestEnv::enforce();
    let config = crate::runtime_layout::config_file_path(env._root.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "server: {}\ndata: {dir: data}\n").unwrap();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "8207").unwrap();
    let fixture = TestPostgresDb::create().await;
    let setup = fixture.connect_and_migrate().await;
    let channel = ChannelId::new(CHANNEL);
    sqlx::query("INSERT INTO agents (id,name,provider,discord_channel_id) VALUES ('reentry-policy','Policy','claude',$1)")
        .bind(CHANNEL.to_string())
        .execute(&setup)
        .await
        .unwrap();
    if !matches!(change, Change::DefaultNode) {
        let local =
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
        sqlx::query("INSERT INTO sessions (session_key,provider,channel_id,instance_id,status,last_heartbeat) VALUES ('claude:reentry-policy','claude',$1,$2,'idle',NOW())")
            .bind(CHANNEL.to_string())
            .bind(local)
            .execute(&setup)
            .await
            .unwrap();
    }
    let proxy = PgWireProxy::start(&fixture.database_url, Duration::ZERO).await;
    let pool = proxy.pool(4);
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared
        .http
        .cached_bot_token
        .set("policy-reentry".into())
        .unwrap();
    shared.restart.reconcile_done.store(true, Relaxed);

    let app = axum::Router::new().fallback(|| async {
        axum::Json(json!({
            "id":"4350", "username":"owner", "discriminator":"0001", "avatar":null,
            "channel_id":CHANNEL.to_string(), "content":"", "author":{
                "id":"4350", "username":"owner", "discriminator":"0001", "avatar":null
            },
            "timestamp":"2026-11-18T00:00:00+00:00", "edited_timestamp":null,
            "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
            "attachments":[], "embeds":[], "pinned":false, "type":0
        }))
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("policy-reentry")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _rest = test_rest::install(http.clone());
    let deps = IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &shared,
        token: "policy-reentry",
    };
    let mut submission = submission_for_admission(channel, MESSAGE);
    submission.request.source_message_ids = vec![MessageId::new(MESSAGE)];
    submission.preserve_on_cancel = true;
    submission.request.preserve_on_cancel = true;
    assert!(matches!(
        super::super::admit_text_intake(&deps, &submission).await,
        IntakeAdmission::Local(_)
    ));
    match change {
        Change::Requirements => {
            sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='reentry-policy'")
                .bind(json!({"os":["windows"]}))
                .execute(&setup)
                .await
                .unwrap();
        }
        Change::DefaultNode => {
            sqlx::query("UPDATE agents SET default_execution_node_id='missing-policy-worker' WHERE id='reentry-policy'")
                .execute(&setup)
                .await
                .unwrap();
        }
        Change::Unchanged => {}
    }
    let persistence = crate::services::discord::queue_dispatch::persistence_context(
        &shared,
        &ProviderKind::Claude,
        channel,
    );
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    let mailbox = shared.mailbox(channel);
    mailbox.v2_ensure(path, namespace).await.unwrap();
    let mut item = queued_intervention(MESSAGE, Vec::new());
    item.text = submission.request.user_text.clone();
    let original = input_ingress::original(&ProviderKind::Claude, channel, None, &item).unwrap();
    assert!(matches!(
        original.admit(&mailbox).await,
        Admission::Accepted(_)
    ));
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let final_calls = Rc::new(Cell::new(0));
    let calls = final_calls.clone();
    let _final = install_final_local_probe(move |submission| {
        assert!(
            submission.owner.is_some(),
            "the real owner start presents its claim"
        );
        calls.set(calls.get() + 1);
        Ok(crate::services::discord::IntakeExecution::Executed)
    });
    proxy.reset_executions();
    let start = crate::services::discord::router::message_handler::start_idle_for_test(
        shared.clone(),
        channel,
        front,
    );
    let result =
        crate::test_support::wait::await_within(start, crate::test_support::wait::WAIT_CAP).await;
    let policy_reads = proxy
        .executions()
        .into_iter()
        .filter(|sql| {
            let sql = sql.to_ascii_lowercase();
            sql.contains("from agents")
                && (sql.contains("execution_requirements")
                    || sql.contains("default_execution_node_id"))
        })
        .count();
    match change {
        Change::Unchanged => {
            assert!(result.is_ok(), "unchanged policy owner start succeeds");
            assert_eq!(final_calls.get(), 1);
        }
        _ => {
            assert_eq!(final_calls.get(), 0, "no native leaf for changed policy");
            assert!(
                result.is_err_and(|error| error.contains("owner_front_not_admitted")),
                "the second real admission blocks the changed policy before local effects"
            );
        }
    }
    assert!(
        policy_reads > 0,
        "the owner start ran a fresh policy admission"
    );
    server.abort();
    drop(shared);
    pool.close().await;
    setup.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn owner_front_reentry_rechecks_changed_requirements_pg() {
    reentry(Change::Requirements).await;
}

#[tokio::test(flavor = "current_thread")]
async fn owner_front_reentry_rechecks_changed_default_node_pg() {
    reentry(Change::DefaultNode).await;
}

#[tokio::test(flavor = "current_thread")]
async fn owner_front_reentry_runs_unchanged_policy_once_pg() {
    reentry(Change::Unchanged).await;
}
