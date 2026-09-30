use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::db::dispatched_sessions::{
    HookSessionUpsert, cleanup_disconnected_sessions_pg, delete_session_by_key_pg,
    gc_stale_thread_sessions_with_probe_pg,
};
use crate::services::platform::tmux::SessionPresence;

const TOKEN: &str = "discord_0123456789abcdef";

fn owner(channel_id: &str) -> HostedOwner {
    HostedOwner {
        provider: "claude".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: channel_id.into(),
        logical_key: "AgentDesk-claude-hosted".into(),
        owner_node: "test-node".into(),
        runtime_root: "/adk/runtime".into(),
    }
}

fn pending(owner: &HostedOwner, nonce: &str) -> HostedExecution {
    let source_ref = SourceRef {
        runtime_root: owner.runtime_root.clone(),
        channel: owner.channel_id.clone(),
        provider: owner.provider.clone(),
        logical_key: owner.logical_key.clone(),
        execution_nonce: nonce.into(),
        initial_source: None,
        baseline_event_seq: None,
    };
    HostedExecution::pending(owner.clone(), nonce.into(), source_ref)
}

fn location(pane_id: &str) -> HostedLocation {
    HostedLocation {
        host: "herdr".into(),
        execution_node: "test-node".into(),
        endpoint_config_key: "herdr.default".into(),
        socket_addr: "/adk/herdr.sock".into(),
        named_session: "agentdesk".into(),
        pane_id: pane_id.into(),
    }
}

fn expected(nonce: &str, root_pid: u32) -> ExpectedExecution {
    ExpectedExecution {
        binding_provider: "claude".into(),
        binding_nonce: nonce.into(),
        root: ProcessStamp {
            pid: root_pid,
            start: "1700000000".into(),
        },
        provider_process: ProcessStamp {
            pid: root_pid + 1,
            start: "1700000001".into(),
        },
        provenance: "launch".into(),
    }
}

fn record(owner: &HostedOwner, nonce: &str, state: HostedState) -> HostedExecution {
    let mut record = pending(owner, nonce);
    if state != HostedState::Pending {
        record.location = Some(location("pane-1"));
        record.expected = Some(expected(nonce, 100));
    }
    record.state = state;
    record
}

fn wire(record: &HostedExecution) -> Value {
    serde_json::to_value(record).unwrap()
}

fn future_schema(owner: &HostedOwner) -> Value {
    let mut raw = wire(&record(owner, "n-future", HostedState::Retired));
    raw["schema"] = json!(2);
    raw
}

#[test]
fn hosted_execution_decode_keeps_unreadable_payloads_unknown() {
    let owner = owner("100");
    assert_eq!(HostedRecord::decode(None), HostedRecord::Legacy);
    assert!(HostedRecord::decode(None).deletable());
    for state in [
        HostedState::Pending,
        HostedState::Bound,
        HostedState::Retired,
    ] {
        let known = record(&owner, "n1", state);
        let decoded = HostedRecord::decode(Some(&wire(&known)));
        assert_eq!(decoded, HostedRecord::Known(known));
        assert_eq!(
            decoded.deletable(),
            state == HostedState::Retired,
            "{state:?}"
        );
    }

    let bound = wire(&record(&owner, "n1", HostedState::Bound));
    let edit = |change: &dyn Fn(&mut Value)| {
        let mut raw = bound.clone();
        change(&mut raw);
        raw
    };
    let retired_with = |change: &dyn Fn(&mut Value)| {
        let mut raw = edit(change);
        raw["state"] = json!("retired");
        raw
    };
    let cases = [
        ("future schema", future_schema(&owner)),
        (
            "lost location key",
            edit(&|raw| drop(raw.as_object_mut().unwrap().remove("location"))),
        ),
        (
            "lost nested key",
            retired_with(&|raw| {
                drop(
                    raw["source_ref"]
                        .as_object_mut()
                        .unwrap()
                        .remove("baseline_event_seq"),
                )
            }),
        ),
        ("extra field", retired_with(&|raw| raw["lease"] = json!(1))),
        (
            "bound without location",
            edit(&|raw| raw["location"] = Value::Null),
        ),
        (
            "source nonce disagrees",
            retired_with(&|raw| raw["source_ref"]["execution_nonce"] = json!("n0")),
        ),
        (
            "evidence nonce disagrees",
            retired_with(&|raw| raw["expected"]["binding_nonce"] = json!("n0")),
        ),
        (
            "non-herdr location",
            retired_with(&|raw| raw["location"]["host"] = json!("tmux")),
        ),
        (
            "blank pane",
            retired_with(&|raw| raw["location"]["pane_id"] = json!(" ")),
        ),
        (
            "unknown state",
            edit(&|raw| raw["state"] = json!("adopted")),
        ),
        ("json null", Value::Null),
        ("array", json!([1, "bound"])),
        ("empty object", json!({})),
    ];
    for (label, raw) in cases {
        let decoded = HostedRecord::decode(Some(&raw));
        assert_eq!(decoded, HostedRecord::Unknown(raw), "{label}");
        assert!(!decoded.deletable(), "{label} must keep its row");
    }
}

async fn insert_session(pool: &PgPool, key: &str, status: &str, thread: Option<&str>) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status, thread_channel_id, last_heartbeat)
         VALUES ($1, 'claude', $2, $3, NOW() - INTERVAL '2 hours') RETURNING id",
    )
    .bind(key)
    .bind(status)
    .bind(thread)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn set_raw(pool: &PgPool, key: &str, raw: Option<Value>) {
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
        .bind(key)
        .bind(raw)
        .execute(pool)
        .await
        .unwrap();
}

async fn remaining_keys(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT session_key FROM sessions ORDER BY session_key")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// One row per record kind; `None` leaves the column NULL (legacy).
fn retention_rows(owner: &HostedOwner, prefix: &str) -> Vec<(String, Option<Value>)> {
    let mut retired_extra = wire(&record(owner, "n1", HostedState::Retired));
    retired_extra["lease"] = json!(1);
    [
        ("a-legacy", None),
        (
            "b-retired",
            Some(wire(&record(owner, "n1", HostedState::Retired))),
        ),
        (
            "c-pending",
            Some(wire(&record(owner, "n1", HostedState::Pending))),
        ),
        (
            "d-bound",
            Some(wire(&record(owner, "n1", HostedState::Bound))),
        ),
        ("e-future", Some(future_schema(owner))),
        ("f-json-null", Some(Value::Null)),
        // Passes the coarse SQL check; only the full decode sees the unknown field.
        ("g-retired-extra", Some(retired_extra)),
    ]
    .into_iter()
    .map(|(name, raw)| (format!("{prefix}{name}"), raw))
    .collect()
}

const KEPT: [&str; 5] = [
    "c-pending",
    "d-bound",
    "e-future",
    "f-json-null",
    "g-retired-extra",
];

#[tokio::test]
async fn hosted_execution_bulk_cleanup_keeps_live_and_unknown_rows_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (key, raw) in retention_rows(&owner("200"), "disc-") {
        insert_session(&pool, &key, "disconnected", None).await;
        set_raw(&pool, &key, raw).await;
    }
    insert_session(&pool, "idle-legacy", "idle", None).await;

    assert_eq!(cleanup_disconnected_sessions_pg(&pool).await.unwrap(), 2);
    let mut expected: Vec<String> = KEPT.iter().map(|name| format!("disc-{name}")).collect();
    expected.push("idle-legacy".into());
    assert_eq!(remaining_keys(&pool).await, expected);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn hosted_execution_thread_gc_keeps_live_unknown_and_changed_rows_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = owner("300");
    let mut rows = retention_rows(&owner, "test-host:AgentDesk-claude-gc-");
    let race = "test-host:AgentDesk-claude-gc-h-retired-then-pending".to_string();
    rows.push((
        race.clone(),
        Some(wire(&record(&owner, "n1", HostedState::Retired))),
    ));
    for (index, (key, raw)) in rows.iter().enumerate() {
        let thread = format!("15006283718294283{index:02}");
        insert_session(&pool, key, "idle", Some(&thread)).await;
        set_raw(&pool, key, raw.clone()).await;
    }

    // A new incarnation installed during the external probe must survive the DELETE.
    let pending_again = wire(&record(&owner, "n2", HostedState::Pending));
    let deleted = gc_stale_thread_sessions_with_probe_pg(&pool, |key| {
        let (pool, race, pending_again) = (pool.clone(), race.clone(), pending_again.clone());
        async move {
            if key == race {
                set_raw(&pool, &key, Some(pending_again)).await;
            }
            SessionPresence::Missing
        }
    })
    .await;
    let mut deleted = deleted;
    deleted.sort();
    assert_eq!(
        deleted,
        [
            "test-host:AgentDesk-claude-gc-a-legacy",
            "test-host:AgentDesk-claude-gc-b-retired"
        ]
    );
    let mut kept: Vec<String> = KEPT
        .iter()
        .map(|name| format!("test-host:AgentDesk-claude-gc-{name}"))
        .collect();
    kept.push(race);
    kept.sort();
    assert_eq!(remaining_keys(&pool).await, kept);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn hosted_execution_explicit_delete_refuses_live_and_unknown_rows_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (key, raw) in retention_rows(&owner("400"), "del-") {
        insert_session(&pool, &key, "idle", None).await;
        set_raw(&pool, &key, raw).await;
    }
    for name in KEPT {
        let key = format!("del-{name}");
        let error = delete_session_by_key_pg(&pool, &key).await.err();
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("hosted execution")),
            "{key}: {error:?}"
        );
    }
    for key in ["del-a-legacy", "del-b-retired"] {
        let result = delete_session_by_key_pg(&pool, key).await.unwrap();
        assert!(result.session_id.is_some(), "{key}");
        assert_eq!(result.deleted, 1, "{key}");
    }
    let kept: Vec<String> = KEPT.iter().map(|name| format!("del-{name}")).collect();
    assert_eq!(remaining_keys(&pool).await, kept);
    pool.close().await;
    db.drop().await;
}

fn upsert<'a>(key: &'a str, channel_id: &'a str) -> HookSessionUpsert<'a> {
    HookSessionUpsert {
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
        channel_id: Some(channel_id),
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: None,
        dispatched_origin: false,
    }
}

async fn seed_canonical(pool: &PgPool, key: &str, channel_id: &str) {
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: TOKEN,
        channel_id,
    };
    crate::db::dispatched_session_canonical_identity::upsert_hook_session_with_identity_pg(
        pool,
        upsert(key, channel_id),
        Some(identity),
    )
    .await
    .unwrap();
}

async fn observe(pool: &PgPool, key: &str) -> HostedObservation {
    match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(observation) => observation,
        other => panic!("{key}: {other:?}"),
    }
}

#[tokio::test]
async fn hosted_execution_lookup_resolves_alias_and_canonical_tuple_to_one_row_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = "1479671301387059300";
    let primary = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-hosted";
    let alias = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-hosted";
    seed_canonical(&pool, primary, channel).await;
    seed_canonical(&pool, alias, channel).await;
    let owner = owner(channel);
    let legacy = observe(&pool, primary).await;
    assert_eq!(legacy.record, HostedRecord::Legacy);
    let installed = install_pending_pg(&pool, &legacy, pending(&owner, "n1")).await;
    assert_eq!(installed, Ok(HostedCasOutcome::Written));

    let identity = |kind, token| CanonicalSessionIdentity {
        kind,
        discord_token_hash: token,
        channel_id: channel,
    };
    let canonical = HostedLookupKey::Canonical {
        provider: "claude",
        identity: identity(SessionIdentityKind::DiscordChannel, TOKEN),
    };
    let by_alias = observe(&pool, alias).await;
    let HostedLookup::Found(by_tuple) = load_hosted_execution_pg(&pool, canonical).await else {
        panic!("canonical tuple lookup");
    };
    assert_eq!(by_alias.session_id(), legacy.session_id());
    assert_eq!(by_alias, by_tuple);
    assert_eq!(by_alias.record, HostedRecord::Known(pending(&owner, "n1")));

    for key in [
        HostedLookupKey::SessionKey(" "),
        HostedLookupKey::Canonical {
            provider: "claude",
            identity: identity(SessionIdentityKind::DiscordChannel, ""),
        },
        HostedLookupKey::Canonical {
            provider: "claude",
            identity: identity(SessionIdentityKind::ScheduledSnapshot, TOKEN),
        },
    ] {
        let lookup = load_hosted_execution_pg(&pool, key).await;
        assert!(
            matches!(lookup, HostedLookup::Unknown(_)),
            "{key:?}: {lookup:?}"
        );
    }
    let absent = HostedLookupKey::SessionKey("claude/absent");
    assert_eq!(
        load_hosted_execution_pg(&pool, absent).await,
        HostedLookup::Missing
    );

    // A record whose owner is another channel is a conflict, not this row's record.
    set_raw(&pool, primary, Some(wire(&pending(&owner_other(), "n1")))).await;
    assert_eq!(
        load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(alias)).await,
        HostedLookup::Conflict(SessionIdentityConflictKind::OwnershipMismatch)
    );
    pool.close().await;
    db.drop().await;
}

fn owner_other() -> HostedOwner {
    owner("1479671301387059999")
}

async fn raw_of(pool: &PgPool, key: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn hosted_execution_cas_writes_only_the_observed_value_and_nonce_pg() {
    use HostedCasOutcome::{Stale, Written};
    use HostedTransitionError as E;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = "1479671301387059400";
    let key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-cas";
    seed_canonical(&pool, key, channel).await;
    let owner = owner(channel);
    let (loc, evidence) = (location("pane-1"), expected("n1", 100));

    let legacy = observe(&pool, key).await;
    assert_eq!(
        install_pending_pg(&pool, &legacy, pending(&owner, "n1")).await,
        Ok(Written)
    );
    assert_eq!(
        install_pending_pg(&pool, &legacy, pending(&owner, "n1b")).await,
        Ok(Stale)
    );
    assert_eq!(raw_of(&pool, key).await, Some(wire(&pending(&owner, "n1"))));

    let fresh = observe(&pool, key).await;
    let other =
        record_launch_evidence_pg(&pool, &fresh, &owner, "n0", loc.clone(), evidence.clone());
    assert_eq!(other.await, Err(E::NonceMismatch));
    assert_eq!(
        bind_pg(&pool, &fresh, &owner, "n1").await,
        Err(E::Incomplete)
    );
    let filled =
        record_launch_evidence_pg(&pool, &fresh, &owner, "n1", loc.clone(), evidence.clone());
    assert_eq!(filled.await, Ok(Written));

    // Stored launch evidence is never replaced by a later observation.
    let recorded = observe(&pool, key).await;
    let before = raw_of(&pool, key).await;
    let replaced = expected("n1", 200);
    let overwrite =
        record_launch_evidence_pg(&pool, &recorded, &owner, "n1", loc.clone(), replaced);
    assert_eq!(overwrite.await, Err(E::ExpectedOverwrite));
    let moved =
        record_launch_evidence_pg(&pool, &recorded, &owner, "n1", location("pane-2"), evidence);
    assert_eq!(moved.await, Err(E::ExpectedOverwrite));
    assert_eq!(raw_of(&pool, key).await, before);
    assert_eq!(bind_pg(&pool, &recorded, &owner, "n1").await, Ok(Written));

    let bound_n1 = observe(&pool, key).await;
    assert_eq!(
        retire_pg(&pool, &bound_n1, &owner_other(), "n1").await,
        Err(E::OwnerMismatch)
    );
    assert_eq!(retire_pg(&pool, &bound_n1, &owner, "n1").await, Ok(Written));
    let retired_n1 = observe(&pool, key).await;
    let reuse = install_pending_pg(&pool, &retired_n1, pending(&owner, "n1")).await;
    assert_eq!(reuse, Err(E::NonceMismatch));
    assert_eq!(
        install_pending_pg(&pool, &retired_n1, pending(&owner, "n2")).await,
        Ok(Written)
    );
    let pending_n2 = observe(&pool, key).await;
    let n2 = expected("n2", 300);
    let filled = record_launch_evidence_pg(&pool, &pending_n2, &owner, "n2", loc, n2);
    assert_eq!(filled.await, Ok(Written));
    assert_eq!(
        bind_pg(&pool, &observe(&pool, key).await, &owner, "n2").await,
        Ok(Written)
    );

    // The old incarnation's retire, stale or freshly observed, leaves the new Bound.
    let bound_n2 = raw_of(&pool, key).await;
    assert_eq!(retire_pg(&pool, &bound_n1, &owner, "n1").await, Ok(Stale));
    let fresh_n2 = observe(&pool, key).await;
    assert_eq!(
        retire_pg(&pool, &fresh_n2, &owner, "n1").await,
        Err(E::NonceMismatch)
    );
    assert_eq!(raw_of(&pool, key).await, bound_n2);

    set_raw(&pool, key, Some(future_schema(&owner))).await;
    let unknown = observe(&pool, key).await;
    assert!(matches!(unknown.record, HostedRecord::Unknown(_)));
    let over_unknown = install_pending_pg(&pool, &unknown, pending(&owner, "n3")).await;
    assert_eq!(over_unknown, Err(E::UnknownRecord));
    assert_eq!(
        retire_pg(&pool, &unknown, &owner, "n-future").await,
        Err(E::UnknownRecord)
    );
    assert_eq!(raw_of(&pool, key).await, Some(future_schema(&owner)));

    // A legacy row without canonical identity never receives a record.
    insert_session(&pool, "claude/legacy-only", "idle", None).await;
    let legacy_only = observe(&pool, "claude/legacy-only").await;
    let refused = install_pending_pg(&pool, &legacy_only, pending(&owner, "n4")).await;
    assert_eq!(refused, Ok(Stale));
    assert_eq!(raw_of(&pool, "claude/legacy-only").await, None);
    pool.close().await;
    db.drop().await;
}
