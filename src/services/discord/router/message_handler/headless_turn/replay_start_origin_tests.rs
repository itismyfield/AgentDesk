//! An origin given to the start route or a started human input rides the execution's own inflight
//! row to the receipt the bridge commits, and an exact retry keeps it on the same execution.
use super::*;
use crate::services::agents::input_result::{InputResult, OriginChannel, lookup};

const AGENT: &str = "origin-headless-agent";

type Receipt = (String, Vec<String>, Option<String>);

async fn receipts_naming(pool: &sqlx::PgPool, origin: &str) -> Vec<Receipt> {
    sqlx::query_as(
        "SELECT turn_id, consumed_origin_ids, terminal_outcome FROM turns
         WHERE consumed_origin_ids @> ARRAY[$1]::TEXT[] ORDER BY turn_id",
    )
    .bind(origin)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The origin the running row names, read through the same lookup the GET route uses.
async fn running_lookup(host: &Host, channel: ChannelId, origin: &str) -> Option<InputResult> {
    let channels = [OriginChannel {
        provider: "claude".into(),
        channel_id: channel.get(),
    }];
    lookup(&host.pool, AGENT, origin, &channels).await.unwrap()
}

/// The start route puts the body's origin on the execution it starts: the running row of the
/// returned turn names it. Its end is the bridge's receipt, shown on the same start path below.
#[test]
fn p7340_start_route_origin_reaches_the_started_execution_pg() {
    let name = concat!(
        module_path!(),
        "::p7340_start_route_origin_reaches_the_started_execution_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_340_401);
        let host = Host::new(&[channel]).await;
        crate::db::agents::insert_agent_channels_for_tests(
            &host.pool,
            AGENT,
            Some(&channel.to_string()),
            None,
        )
        .await;
        let config = crate::config::Config::default();
        let broadcast_tx = crate::eventbus::new_broadcast();
        let state = crate::app_state::AppState {
            pg_pool: Some(host.pool.clone()),
            engine: crate::engine::PolicyEngine::new(&config).unwrap(),
            config: Arc::new(config),
            batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
            broadcast_tx,
            health_registry: Some(host.registry.clone()),
            cluster_instance_id: None,
        };
        let body = crate::server::routes::agents::StartAgentTurnBody {
            prompt: "caller rendered original".into(),
            metadata: None,
            source: Some("imessage".into()),
            provider: None,
            channel_id: None,
            dm_user_id: None,
            origin_id: Some("G-start-route".into()),
        };
        let (status, axum::Json(answer)) = crate::server::routes::agents::start_agent_turn(
            axum::extract::State(state),
            axum::extract::Path(AGENT.to_string()),
            axum::Json(body),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{answer}");
        assert_eq!(answer["status"], "started", "{answer}");
        let turn_id = answer["turn_id"].as_str().unwrap().to_string();
        assert_eq!(
            running_lookup(&host, channel, "G-start-route").await,
            Some(InputResult::Running {
                channel_id: channel.get(),
                execution_id: turn_id,
            }),
            "the started execution's own row names the origin"
        );
        host.close().await;
    });
}

/// A human input that starts a turn puts its origin on the execution it starts, through the
/// same headless start whose end the exact-retry case below follows to the receipt.
#[test]
fn p7340_delivered_start_origin_reaches_the_started_execution_pg() {
    let name = concat!(
        module_path!(),
        "::p7340_delivered_start_origin_reaches_the_started_execution_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_340_403);
        let host = Host::new(&[channel]).await;
        host.shared.settings.write().await.owner_user_id = Some(42);
        let request = health::HumanInputRequest {
            channel_id: channel,
            provider: ProviderKind::Claude,
            text: "caller rendered original".into(),
            author_id: 42,
            source: "imessage".into(),
            metadata: Some(serde_json::json!({"human_input": {"origin_id": "G-deliver"}})),
            channel_name_hint: None,
        };
        let delivered = health::deliver_human_input(&host.registry, request).await;
        let Ok(health::HumanInputDelivery::Started { turn_id }) = delivered else {
            panic!("an idle channel starts the input: {delivered:?}");
        };
        assert_eq!(
            running_lookup(&host, channel, "G-deliver").await,
            Some(InputResult::Running {
                channel_id: channel.get(),
                execution_id: turn_id,
            }),
            "the started execution's own row names the delivered origin"
        );
        host.close().await;
    });
}

#[test]
fn p7340_exact_retry_keeps_the_origin_on_the_same_execution_pg() {
    let name = concat!(
        module_path!(),
        "::p7340_exact_retry_keeps_the_origin_on_the_same_execution_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_340_405);
        let host = Host::new(&[channel]).await;
        host.stale_cli(None);
        let _active = crate::services::discord::router::intake_dispatch::replay_admission::test_activation::activate(channel.get());
        let reservation = health::reserve_headless_agent_turn(channel);
        let turn_id = reservation.turn_id().to_owned();
        let metadata = serde_json::json!({
            "routine_id": "origin-routine", "routine_run_id": "origin-run", "turn_id": turn_id,
            "execution_strategy": "fresh", "execution_mode": "process", "delivery_bot": "claude",
            "silent": false, "human_input": {"origin_id": "G-exact"}
        });
        let outcome = host
            .start(channel, "imessage", Some(metadata), reservation)
            .await;
        assert_eq!(outcome.unwrap().turn_id, turn_id);
        host.disposition("classified_normal").await;
        host.settled(channel).await;
        assert_eq!(host.starts(), 2, "one exact retry, same headless path");
        assert_eq!(
            receipts_naming(&host.pool, "G-exact").await,
            vec![(
                turn_id.clone(),
                vec!["G-exact".to_string()],
                Some("completed".to_string())
            )],
            "the retried attempt ends the one execution, still naming the origin"
        );
        host.close().await;
    });
}
