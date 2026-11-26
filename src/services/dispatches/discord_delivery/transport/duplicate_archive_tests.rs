use super::archive_duplicate_slot_threads;
use axum::{Json, Router, extract::State, routing::get};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const PARENT: u64 = 8118_301;
const THREAD: &str = "8118302";

#[tokio::test]
async fn s2b1_duplicate_archive_requires_confirmed_tmux_parent_and_legacy_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let database = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let patches = Arc::new(AtomicUsize::new(0));
    let api = Router::new()
        .route(
            "/channels/{thread}",
            get(|| async {
                Json(json!({
                    "parent_id": PARENT.to_string(),
                    "thread_metadata": {"archived": false}
                }))
            })
            .patch(|State(count): State<Arc<AtomicUsize>>| async move {
                count.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::OK
            }),
        )
        .with_state(patches.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async { axum::serve(listener, api).await.unwrap() });
    let client = reqwest::Client::new();
    let candidates = [THREAD.to_string()];
    let archive = || {
        archive_duplicate_slot_threads(
            &client,
            "test",
            &base,
            Some(&pool),
            PARENT,
            "keep",
            &candidates,
        )
    };
    {
        let _boot = crate::config::session_hosts::force_for_test(Some("local"), &[]);
        archive().await;
        assert_eq!(patches.load(Ordering::SeqCst), 1, "legacy control");
    }
    {
        let _boot =
            crate::config::session_hosts::force_for_test(Some("local"), &[(PARENT, "local")]);
        archive().await;
        assert_eq!(patches.load(Ordering::SeqCst), 1, "rowless child of Herdr");
    }
    {
        use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};
        let _boot = crate::config::session_hosts::force_for_test(Some("local"), &[]);
        let _relation = force_relation_for_test(PARENT, Relation::Unresolved);
        archive().await;
        assert_eq!(patches.load(Ordering::SeqCst), 1, "unknown parent policy");
    }
    let _boot = crate::config::session_hosts::force_for_test(Some("local"), &[]);
    let owner = crate::db::dispatched_sessions::hosted_execution::HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "duplicate-archive-token".into(),
        channel_id: THREAD.into(),
        logical_key: "duplicate-archive-child".into(),
        owner_node: "local".into(),
        runtime_root: "/fixture".into(),
    };
    let nonce = "6a62d697beb64bc48ec07484d99d7962";
    let execution = crate::db::dispatched_sessions::hosted_execution::HostedExecution::pending(
        owner.clone(),
        nonce.into(),
        crate::db::dispatched_sessions::hosted_execution::SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: owner.provider.clone(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
    );
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, thread_channel_id, hosted_execution) VALUES ($1, $2, 'idle', 'discord_channel', $3, $4, $4, $5)")
        .bind(&owner.logical_key)
        .bind(&owner.provider)
        .bind(&owner.discord_token_hash)
        .bind(&owner.channel_id)
        .bind(serde_json::to_value(execution).unwrap())
        .execute(&pool).await.unwrap();
    archive().await;
    assert_eq!(
        patches.load(Ordering::SeqCst),
        1,
        "Herdr row under a Tmux parent"
    );

    server.abort();
    pool.close().await;
    database.drop().await;
}
