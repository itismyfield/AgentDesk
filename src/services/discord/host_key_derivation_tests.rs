use serde_json::Value;
use sqlx::PgPool;

use super::*;
use crate::db::dispatched_session_canonical_identity::upsert_hook_session_with_identity_pg;
use crate::db::dispatched_sessions::HookSessionUpsert;

/// Writes the sessions row `key` the way a turn's status hook does, with the canonical
/// `(provider, hash, channel)` tuple if a hash is given; `raw` is its hosted record.
pub(crate) async fn seed_row(
    pool: &PgPool,
    provider: &str,
    hash: Option<&str>,
    key: &str,
    channel_id: u64,
    raw: Option<Value>,
) {
    let channel = channel_id.to_string();
    let params = HookSessionUpsert {
        session_key: key,
        instance_id: Some("test-node"),
        agent_id: None,
        provider,
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
    let identity = hash.map(|hash| CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: hash,
        channel_id: &channel,
    });
    upsert_hook_session_with_identity_pg(pool, params, identity)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
        .bind(key)
        .bind(raw)
        .execute(pool)
        .await
        .unwrap();
}

fn label(lookup: &HostedLookup) -> String {
    match lookup {
        HostedLookup::Found(found) => format!("Found({})", found.session_id()),
        other => format!("{other:?}"),
    }
}

const CLAUDE: ProviderKind = ProviderKind::Claude;

// Candidates come from registered hashes under exact keys only: no hash is Unknown, a
// row under one hash is Found, two bots' rows are a Conflict, and a name match is Missing.
#[tokio::test]
async fn derive_lookup_tries_every_registered_hash_by_exact_key_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = |n: u64| 1_479_671_301_387_060_000 + n;
    let hashes = |list: &[&str]| list.iter().map(|h| h.to_string()).collect::<Vec<_>>();
    let key = |hash: &str, provider: &ProviderKind, tmux: &str| {
        super::super::adk_session::build_namespaced_session_key(hash, provider, tmux)
    };
    let seed = |hash: Option<&'static str>, key: String, n: u64| {
        let pool = pool.clone();
        async move { seed_row(&pool, "claude", hash, &key, channel(n), None).await }
    };
    seed(Some("h-a"), key("h-a", &CLAUDE, "w2b-one"), 1).await;
    for hash in ["h-a", "h-b"] {
        seed(Some(hash), key(hash, &CLAUDE, "w2b-two"), 2).await;
    }
    // A row keyed under a host name the probe no longer produces; the writer's current key
    // joins such a row as its alias.
    seed(Some("h-a"), "claude/h-a/old-host:w2b-moved".to_string(), 3).await;
    seed(
        Some("h-a"),
        "claude/h-a/old-host:w2b-aliased".to_string(),
        4,
    )
    .await;
    seed(Some("h-a"), key("h-a", &CLAUDE, "w2b-aliased"), 4).await;
    let host = crate::services::platform::hostname_short();
    seed(Some("h-a"), key("h-a", &CLAUDE, "w2b-near-dev"), 5).await;
    seed(Some("h-a"), key("h-a", &CLAUDE, "w2b-NEAR"), 6).await;
    seed(None, format!("{host}:w2b-near"), 7).await;

    let derive = |list: Vec<String>, provider: ProviderKind, n: u64, tmux: &'static str| {
        let pool = pool.clone();
        async move { derive_hosted_lookup(&pool, &list, &provider, channel(n), tmux).await }
    };
    let found = |lookup: &HostedLookup| matches!(lookup, HostedLookup::Found(_));
    let none = derive(hashes(&[]), CLAUDE, 1, "w2b-one").await;
    assert!(matches!(none, HostedLookup::Unknown(_)), "{}", label(&none));
    let blank = derive(hashes(&["h-a"]), CLAUDE, 1, " ").await;
    assert!(
        matches!(blank, HostedLookup::Unknown(_)),
        "{}",
        label(&blank)
    );
    let cases = [
        ("one hash", hashes(&["h-a"]), CLAUDE, 1, "w2b-one", true),
        (
            "hash without a row",
            hashes(&["h-a", "h-z"]),
            CLAUDE,
            1,
            "w2b-one",
            true,
        ),
        (
            "rotated token",
            hashes(&["h-new"]),
            CLAUDE,
            1,
            "w2b-one",
            false,
        ),
        (
            "host renamed",
            hashes(&["h-a"]),
            CLAUDE,
            3,
            "w2b-moved",
            true,
        ),
        (
            "host renamed, other channel",
            hashes(&["h-a"]),
            CLAUDE,
            9,
            "w2b-moved",
            false,
        ),
        (
            "alias of an old key",
            hashes(&["h-a"]),
            CLAUDE,
            9,
            "w2b-aliased",
            true,
        ),
        (
            "near names only",
            hashes(&["h-a"]),
            CLAUDE,
            9,
            "w2b-near",
            false,
        ),
        (
            "caller provider decides",
            hashes(&["h-a"]),
            ProviderKind::Codex,
            9,
            "w2b-one",
            false,
        ),
    ];
    for (case, list, provider, n, tmux, expected) in cases {
        let lookup = derive(list, provider, n, tmux).await;
        assert_eq!(found(&lookup), expected, "{case}: {}", label(&lookup));
        if !expected {
            assert_eq!(lookup, HostedLookup::Missing, "{case}: Missing, not legacy");
        }
    }
    let two_bots = derive(hashes(&["h-a", "h-b"]), CLAUDE, 2, "w2b-two").await;
    assert_eq!(
        two_bots,
        HostedLookup::Conflict(SessionIdentityConflictKind::EvidenceDivergence),
        "two bots' rows for one channel and name"
    );
    let one_bot = derive(hashes(&["h-b"]), CLAUDE, 2, "w2b-two").await;
    assert!(found(&one_bot), "{}", label(&one_bot));

    pool.close().await;
    let failed = derive(hashes(&["h-a"]), CLAUDE, 1, "w2b-one").await;
    assert!(
        matches!(failed, HostedLookup::Unknown(_)),
        "{}",
        label(&failed)
    );
    db.drop().await;
}

// A conflict decides first, then a failed read, since either may hide a row another
// candidate missed; found rows then need one id, then one reading of it.
#[tokio::test]
async fn merge_lets_conflicts_and_failed_reads_decide_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (first, second) = ("claude/h-a/host:w2b-first", "claude/h-a/host:w2b-second");
    seed_row(&pool, "claude", Some("h-a"), first, 11, None).await;
    seed_row(&pool, "claude", Some("h-a"), second, 12, None).await;
    let load = |key: &'static str| {
        let pool = pool.clone();
        async move { load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(key)).await }
    };
    let (a, b) = (load(first).await, load(second).await);
    sqlx::query("UPDATE sessions SET hosted_execution = '{}'::jsonb WHERE session_key = $1")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    let a_changed = load(first).await;
    assert!(
        matches!(a_changed, HostedLookup::Found(_)),
        "{}",
        label(&a_changed)
    );
    db.drop().await;

    let unknown = || HostedLookup::Unknown("read failed".to_string());
    let owner = || HostedLookup::Conflict(SessionIdentityConflictKind::OwnershipMismatch);
    let diverged = HostedLookup::Conflict(SessionIdentityConflictKind::EvidenceDivergence);
    let changed = HostedLookup::Unknown("record changed between candidate reads".to_string());
    let missing = || HostedLookup::Missing;
    let cases = [
        ("found then failed", vec![a.clone(), unknown()], unknown()),
        ("failed then found", vec![unknown(), a.clone()], unknown()),
        (
            "failed then conflict",
            vec![unknown(), owner(), a.clone()],
            owner(),
        ),
        (
            "two rows",
            vec![a.clone(), missing(), b.clone()],
            diverged.clone(),
        ),
        (
            "one row read twice apart",
            vec![a.clone(), a_changed.clone()],
            changed,
        ),
        (
            "rows differ before readings",
            vec![a.clone(), a_changed, b],
            diverged,
        ),
        (
            "one row read alike",
            vec![missing(), a.clone(), a.clone()],
            a,
        ),
        ("all missing", vec![missing(), missing()], missing()),
    ];
    for (case, lookups, expected) in cases {
        let merged = merge_lookups(lookups);
        assert_eq!(merged, expected, "{case}: {}", label(&merged));
    }
}
