use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[tokio::test(flavor = "current_thread")]
async fn registered_receipt_raw_proof_keeps_exact_keys_empty_queries_and_invalid_rows_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('r', 'r.js', 'r')")
        .execute(&pool)
        .await
        .unwrap();
    let name = "AgentDesk-claude-routine-registered-proof";
    let absent = "AgentDesk-claude-routine-absent-proof";
    let codex_name = "AgentDesk-codex-routine-provider-proof";
    let runtime = |provider, token: &str| ReceiptRuntime {
        provider,
        token_hash: token.into(),
    };
    let runtimes = vec![
        runtime(ProviderKind::Claude, "a"),
        runtime(ProviderKind::Claude, "b"),
        runtime(ProviderKind::Claude, "a"),
        runtime(ProviderKind::Codex, "c"),
    ];
    let key = |token, provider: &ProviderKind, name| {
        adk_session::build_namespaced_session_key(token, provider, name)
    };
    let first = key("a", &ProviderKind::Claude, name);
    let second = key("b", &ProviderKind::Claude, name);
    let rogue = key("unregistered", &ProviderKind::Claude, name);
    let wrong_host = format!(
        "claude/a/not-{}:{name}",
        crate::services::platform::hostname_short()
    );
    let wrong_provider = key("c", &ProviderKind::Codex, name);
    for (id, owned, turn, channel) in [
        ("z-valid", first.as_str(), Some("turn-z"), "69184902"),
        ("a-valid", first.as_str(), Some("turn-a"), "69184901"),
        ("invalid", second.as_str(), None, "bad-channel"),
        ("rogue", rogue.as_str(), Some("turn"), "69184901"),
        ("wrong-host", wrong_host.as_str(), Some("turn"), "69184901"),
        (
            "wrong-provider",
            wrong_provider.as_str(),
            Some("turn"),
            "69184901",
        ),
    ] {
        sqlx::query("INSERT INTO routine_runs (id, routine_id, status, owned_tmux_session, turn_id, result_json) VALUES ($1, 'r', 'succeeded', $2, $3, $4)")
            .bind(id).bind(owned).bind(turn)
            .bind(serde_json::json!({"channel_id": channel}))
            .execute(&pool).await.unwrap();
    }
    let proof = routine_turn_evidence(
        &pool,
        &runtimes,
        &[
            name.into(),
            absent.into(),
            codex_name.into(),
            "claude".into(),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        proof.keys.len(),
        5,
        "provider/token pairs were dropped or duplicate runtimes duplicated a key"
    );
    assert_eq!(
        proof.keys[&first]
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["a-valid", "z-valid"]
    );
    assert_eq!(
        proof.keys[&first].channels(),
        [69_184_901, 69_184_902].into_iter().collect()
    );
    assert!(!proof.keys[&first].invalid());
    assert!(proof.keys[&second].invalid());
    assert_eq!(
        proof.keys[&second].rows[0].channel_id.as_deref(),
        Some("bad-channel")
    );
    assert!(
        proof.keys[&key("a", &ProviderKind::Claude, absent)]
            .rows
            .is_empty()
    );
    assert!(
        proof.keys[&key("b", &ProviderKind::Claude, absent)]
            .rows
            .is_empty()
    );
    assert!(
        proof.keys[&key("c", &ProviderKind::Codex, codex_name)]
            .rows
            .is_empty()
    );
    assert!(!proof.keys.contains_key(&rogue));
    assert!(!proof.keys.contains_key(&wrong_host));
    assert!(!proof.keys.contains_key(&wrong_provider));
    assert!(proof.self_claim(name, 69_184_901));
    assert_eq!(
        proof,
        routine_turn_evidence(
            &pool,
            &runtimes,
            &[
                name.into(),
                absent.into(),
                codex_name.into(),
                "claude".into()
            ]
        )
        .await
        .unwrap()
    );
    pool.close().await;
    db.drop().await;
}
