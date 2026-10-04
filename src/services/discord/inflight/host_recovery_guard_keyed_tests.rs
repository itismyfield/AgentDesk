use serde_json::Value;
use sqlx::PgPool;

use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{
    TOKEN, future_schema, owner, pending, record, wire,
};
use crate::services::session_host::HostedRuntimeLocator;

/// Seeds the sessions row `claude/<token>/mac-mini:<name>` for `channel_id`;
/// `raw` is its hosted record, `None` for SQL NULL. Returns the row's key.
pub(in crate::services::discord) async fn seed_session_row(
    pool: &PgPool,
    name: &str,
    channel_id: u64,
    raw: Option<Value>,
) -> String {
    let key = format!("claude/{TOKEN}/mac-mini:{name}");
    seed_session_row_keyed(pool, &key, channel_id, raw).await;
    key
}

/// [`seed_session_row`] under a key the caller built.
pub(in crate::services::discord) async fn seed_session_row_keyed(
    pool: &PgPool,
    key: &str,
    channel_id: u64,
    raw: Option<Value>,
) {
    seed_session_row_hashed(pool, key, channel_id, TOKEN, raw).await;
}

/// [`seed_session_row_keyed`] with the channel identity of bot hash `hash`.
pub(in crate::services::discord) async fn seed_session_row_hashed(
    pool: &PgPool,
    key: &str,
    channel_id: u64,
    hash: &str,
    raw: Option<Value>,
) {
    let channel = channel_id.to_string();
    let params = HookSessionUpsert {
        session_key: key,
        instance_id: Some("test-node"),
        agent_id: None,
        provider: "claude",
        status: "idle",
        session_info: None,
        model: None,
        tokens: None,
        cwd: None,
        active_dispatch_id: None,
        thread_channel_id: None,
        channel_id: Some(&channel),
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: None,
        dispatched_origin: false,
    };
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: hash,
        channel_id: &channel,
    };
    upsert_hook_session_with_identity_pg(pool, params, Some(identity))
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
        .bind(key)
        .bind(raw)
        .execute(pool)
        .await
        .unwrap();
}

fn inflight_row(channel_id: u64, tmux: &str, host_kind: HostKind) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        Some("adk-cc".to_string()),
        222,
        333,
        444,
        "hello".to_string(),
        None,
        Some(tmux.to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    );
    state.host_locator = Some(PersistedHostLocator::Known(HostedRuntimeLocator {
        execution_node: None,
        host_kind,
        host_session_id: format!("{tmux}-host"),
        pane: Some("w1-1".to_string()),
    }));
    state
}

// The gate the retry, startup and recreate teardowns take: sessions row, `.host_kind`
// marker and inflight row as stored, and only a found legacy row with no trace admits.
#[tokio::test]
async fn keyed_teardown_admits_only_a_found_legacy_row_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = |n: u64| 1_479_671_301_387_059_400 + n;
    let seed = |name: &'static str, n: u64, raw: Option<Value>| {
        let pool = pool.clone();
        async move { seed_session_row(&pool, name, channel(n), raw).await }
    };
    let owned = |n: u64| owner(&channel(n).to_string());
    let legacy = seed("p4c3w1-legacy", 1, None).await;
    let herdr_marked = seed("p4c3w1-herdr-marker", 2, None).await;
    let tmux_marked = seed("p4c3w1-tmux-marker", 3, None).await;
    let herdr_inflight = seed("p4c3w1-herdr-inflight", 4, None).await;
    let other_inflight = seed("p4c3w1-other-inflight", 5, None).await;
    let unreadable_inflight = seed("p4c3w1-unreadable-inflight", 6, None).await;
    let bound = wire(&record(&owned(7), "n1", HostedState::Bound));
    let bound = seed("p4c3w1-bound", 7, Some(bound)).await;
    let pending_key = seed("p4c3w1-pending", 8, Some(wire(&pending(&owned(8), "n2")))).await;
    let future = seed("p4c3w1-future", 9, Some(future_schema(&owned(9)))).await;
    let foreign = wire(&pending(&owned(99), "n3"));
    let foreign = seed("p4c3w1-foreign", 10, Some(foreign)).await;

    let marker = |name: &str, kind: &str| {
        let path = crate::services::tmux_common::session_temp_path(name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(path, kind).unwrap();
    };
    marker("p4c3w1-herdr-marker", "herdr");
    marker("p4c3w1-tmux-marker", "tmux");
    let herdr_row = inflight_row(channel(4), "p4c3w1-herdr-inflight", HostKind::Herdr);
    super::super::save_inflight_state(&herdr_row).unwrap();
    let other_row = inflight_row(channel(5), "p4c3w1-another-session", HostKind::Herdr);
    super::super::save_inflight_state(&other_row).unwrap();
    let root = super::super::inflight_runtime_root().unwrap();
    let path = super::super::store::inflight_state_path(&root, &ProviderKind::Claude, channel(6));
    std::fs::create_dir_all(path).unwrap();

    let missing = format!("claude/{TOKEN}/mac-mini:p4c3w1-no-row");
    let cases: [(&str, Option<&str>, &str, u64, bool); 13] = [
        ("found legacy row", Some(&legacy), "p4c3w1-legacy", 1, true),
        (
            "tmux marker",
            Some(&tmux_marked),
            "p4c3w1-tmux-marker",
            3,
            true,
        ),
        (
            "inflight row of another session",
            Some(&other_inflight),
            "p4c3w1-other-inflight",
            5,
            true,
        ),
        (
            "Herdr marker",
            Some(&herdr_marked),
            "p4c3w1-herdr-marker",
            2,
            false,
        ),
        (
            "Herdr inflight locator",
            Some(&herdr_inflight),
            "p4c3w1-herdr-inflight",
            4,
            false,
        ),
        (
            "unreadable inflight row",
            Some(&unreadable_inflight),
            "p4c3w1-unreadable-inflight",
            6,
            false,
        ),
        ("bound Herdr record", Some(&bound), "p4c3w1-bound", 7, false),
        (
            "pending Herdr record",
            Some(&pending_key),
            "p4c3w1-pending",
            8,
            false,
        ),
        ("future record", Some(&future), "p4c3w1-future", 9, false),
        ("foreign owner", Some(&foreign), "p4c3w1-foreign", 10, false),
        ("missing row", Some(&missing), "p4c3w1-no-row", 11, false),
        ("blank key", Some(" "), "p4c3w1-legacy", 1, false),
        ("no key", None, "p4c3w1-legacy", 1, false),
    ];
    let claude = ProviderKind::Claude;
    for (label, key, tmux, n, admitted) in cases {
        let cleared = clear_channel_session(Some(&pool), &claude, channel(n), key, tmux, label)
            .await
            .map(|session| session.name().to_string());
        assert_eq!(cleared.as_deref(), admitted.then_some(tmux), "{label}");
    }
    // The admitted legacy row is kept once its channel is Herdr-configured.
    let _hosts = crate::config::session_hosts::force_for_test(None, &[(channel(1), "mac-mini")]);
    let tmux = "p4c3w1-legacy";
    let kept = clear_channel_session(Some(&pool), &claude, channel(1), Some(&legacy), tmux, "c");
    assert_eq!(kept.await.map(|s| s.name().to_string()), None, "configured");
    let no_pool = clear_channel_session(None, &claude, channel(1), Some(&legacy), "p", "x").await;
    assert_eq!(no_pool, None, "no pool is not a legacy answer");
    pool.close().await;
    let failed = clear_channel_session(Some(&pool), &claude, channel(1), Some(&legacy), "p", "x");
    assert_eq!(failed.await, None, "a failed lookup is not a legacy answer");
    db.drop().await;
}
