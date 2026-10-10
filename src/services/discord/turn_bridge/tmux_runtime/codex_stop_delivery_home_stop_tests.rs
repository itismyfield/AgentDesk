//! On a real Herdr turn the holder runs a forwarded stop as its own `/stop` only for a trusted
//! forward naming its fresh row; the gateway reaches it over HTTP from its own thread.
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;

use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::cluster::{channel_home, home_availability};
use crate::services::discord::commands::stop::run_holder_stop_on;
use crate::services::session_forwarding::ForwardCallerContext;
use crate::services::session_forwarding::home_stop::{self, GatewayStop};

const HOLDER: &str = "mini";

fn holder_context(case: &Case) -> ForwardCallerContext {
    ForwardCallerContext {
        pg_pool: case.shared.pg_pool.clone(),
        config: Arc::new(crate::config::Config::default()),
        cluster_instance_id: Some(HOLDER.into()),
    }
}

fn trusted(owner: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-agentdesk-forwarded-by", HeaderValue::from_static("gw"));
    headers.insert(
        "x-agentdesk-session-owner",
        HeaderValue::from_str(owner).unwrap(),
    );
    headers
}

fn body(case: &Case, channel: u64, holder: &str, epoch: i64, provider: &str) -> Vec<u8> {
    let _ = case;
    home_stop::tests::request_body(channel, holder, epoch, provider)
}

async fn receive(case: &Case, headers: &HeaderMap, body: &[u8]) -> (StatusCode, Value) {
    let shared = Arc::clone(&case.shared);
    let run = |provider: ProviderKind, channel: u64| async move {
        Some(run_holder_stop_on(&shared, &provider, ChannelId::new(channel)).await)
    };
    home_stop::receive(&holder_context(case), headers, body, run).await
}

/// The holder's row and gate at epoch 1, as its renewal leaves them.
async fn hold(case: &Case) -> home_availability::Registration {
    let pool = case.shared.pg_pool.as_ref().unwrap();
    sqlx::query("INSERT INTO o_channel_homes (channel_id, provider, state, holder, epoch, renewed_at) VALUES ($1, $2, 'worker', $3, 1, NOW())")
        .bind(case.channel.to_string()).bind(case.provider.as_str()).bind(HOLDER)
        .execute(pool).await.unwrap();
    channel_home::register_for_test(case.channel.get(), Some(HomeState::Worker));
    home_availability::install(case.provider.as_str(), Ok(()), Default::default)
}

/// [`with_cases`] on channels no other stop test touches, so no earlier test's gate state leaks in.
fn with_home_cases(mut check: impl FnMut(&Case, &tokio::runtime::Runtime)) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    let db = runtime.block_on(crate::db::auto_queue::test_support::TestPostgresDb::create());
    let pool = runtime.block_on(db.connect_and_migrate());
    for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        let case = runtime.block_on(Case::new(&pool, root.path(), provider, 40 + n as u64));
        let _registry = case.rig.registry_on_this_thread();
        check(&case, &runtime);
    }
    runtime.block_on(pool.close());
    runtime.block_on(db.drop());
}

#[test]
fn t9_f1_holder_runs_only_a_trusted_fresh_request_and_refusals_send_nothing() {
    with_home_cases(|case, runtime| {
        let _on = runtime.block_on(hold(case));
        let channel = case.channel.get();
        let provider = case.provider.as_str();
        let other = if case.provider == ProviderKind::Claude {
            "codex"
        } else {
            "claude"
        };
        let _other_on = home_availability::install(other, Ok(()), Default::default);
        let pool = case.shared.pg_pool.as_ref().unwrap();
        let refusals: [(HeaderMap, Vec<u8>, StatusCode, Option<&str>); 7] = [
            (
                HeaderMap::new(),
                body(case, channel, HOLDER, 1, provider),
                StatusCode::FORBIDDEN,
                None,
            ),
            (
                trusted(HOLDER),
                body(case, channel, HOLDER, 2, provider),
                StatusCode::OK,
                Some("epoch_mismatch"),
            ),
            (
                trusted("other"),
                body(case, channel, "other", 1, provider),
                StatusCode::OK,
                Some("holder_mismatch"),
            ),
            (
                trusted(HOLDER),
                body(case, channel, HOLDER, 1, other),
                StatusCode::OK,
                Some("home_provider_mismatch"),
            ),
            (
                trusted(HOLDER),
                body(case, channel + 1000, HOLDER, 1, provider),
                StatusCode::OK,
                Some("home_absent"),
            ),
            (
                trusted("other"),
                body(case, channel, HOLDER, 1, provider),
                StatusCode::CONFLICT,
                None,
            ),
            (
                trusted(HOLDER),
                body(case, channel, HOLDER, 1, provider),
                StatusCode::OK,
                Some("stale_session_owner"),
            ),
        ];
        for (n, (headers, request, status, reason)) in refusals.into_iter().enumerate() {
            if n == 6 {
                runtime.block_on(sqlx::query("INSERT INTO sessions (session_key, provider, status, channel_id, instance_id) VALUES ('stale:s', $1, 'turn_active', $2, 'stale-node')")
                    .bind(provider).bind(channel.to_string()).execute(pool)).unwrap();
            }
            let (got, answer) = runtime.block_on(receive(case, &headers, &request));
            assert_eq!(case.escapes(), 0, "refusal {n} sent nothing: {answer}");
            assert!(!case.token.cancelled.load(Ordering::SeqCst));
            assert_eq!(got, status, "refusal {n}: {answer}");
            if let Some(reason) = reason {
                assert_eq!(answer["reason"], reason, "refusal {n}");
                assert_eq!(answer["outcome"], "refused");
            }
        }
        runtime
            .block_on(
                sqlx::query("DELETE FROM sessions WHERE session_key = 'stale:s'").execute(pool),
            )
            .unwrap();

        // The gateway on its own thread, registry, runtime and pool reaches the holder over HTTP.
        let shared = Arc::clone(&case.shared);
        let context = holder_context(case);
        let (origin, server) = runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}/", listener.local_addr().unwrap());
            let app = axum::Router::new().route(
                "/api/internal/home-stop/v1",
                axum::routing::post(move |headers: HeaderMap, body: axum::body::Bytes| {
                    let (shared, context) = (Arc::clone(&shared), context.clone());
                    async move {
                        let run = |provider: ProviderKind, channel: u64| async move {
                            Some(
                                run_holder_stop_on(&shared, &provider, ChannelId::new(channel))
                                    .await,
                            )
                        };
                        let (status, answer) =
                            home_stop::receive(&context, &headers, &body, run).await;
                        (status, axum::Json(answer))
                    }
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            (origin, server)
        });
        let options = (*pool.connect_options()).clone();
        let provider_name = provider.to_owned();
        let gateway = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                assert!(
                    channel_home::registered_channel(channel).is_none(),
                    "own registry"
                );
                let _on = home_availability::install(&provider_name, Ok(()), Default::default);
                home_stop::TEST_ORIGINS
                    .with(|origins| origins.borrow_mut().insert(HOLDER.into(), origin));
                let pool = sqlx::PgPool::connect_with(options).await.unwrap();
                let context = ForwardCallerContext {
                    pg_pool: Some(pool.clone()),
                    config: Arc::new(crate::config::Config::default()),
                    cluster_instance_id: Some("gw".into()),
                };
                let stopped = home_stop::gateway_stop(&context, channel, &provider_name).await;
                pool.close().await;
                stopped
            })
        });
        let stopped = runtime.block_on(async {
            while !gateway.is_finished() {
                tokio::task::yield_now().await;
            }
            gateway.join().unwrap()
        });
        server.abort();
        let GatewayStop::Confirmed(answer) = stopped else {
            panic!("gateway result: {stopped:?}");
        };
        assert_eq!(answer["outcome"], "herdr", "{answer}");
        assert_eq!(answer["delivery"], "sent", "{answer}");
        assert_eq!(answer["holder"], HOLDER);
        assert_eq!(answer["home_epoch"], 1);
        assert_eq!(answer["terminal_confirmed"], false);
        assert_eq!(case.escapes(), 1, "the holder's own /stop sent one Escape");
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        let home = channel_home::registered(&case.channel.to_string()).unwrap();
        assert_eq!(
            home.commands_in_flight(),
            0,
            "the permit ended with the stop"
        );
    });
}

#[test]
fn slash_stop_asks_the_home_before_the_legacy_owner_forward_and_ends_there() {
    let source = include_str!("../../commands/control.rs");
    let body = source.split("async fn cmd_stop").nth(1).unwrap();
    let body = body
        .split("pub(super) fn parse_queued_message_id")
        .next()
        .unwrap();
    let home = body.find("super::stop::gateway_stop_reply(").unwrap();
    assert_eq!(body.matches("gateway_stop_reply(").count(), 1);
    assert!(home < body.find("forward_remote_cancel_if_needed(").unwrap());
    let answered = &body[home..body.find("forward_remote_cancel_if_needed(").unwrap()];
    assert!(answered.contains("ctx.say(reply)") && answered.contains("return Ok(());"));
}
