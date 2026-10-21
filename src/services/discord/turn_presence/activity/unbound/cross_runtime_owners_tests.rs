use super::super::routine_owner::{KeyReceipts, ReceiptRow};
use super::*;

const NAME: &str = "AgentDesk-codex-routine-cross-owner";
const TARGET: u64 = 69_184_101;
const FOREIGN: u64 = 69_184_901;

fn receipt(key: &str, channels: &[u64]) -> (String, KeyReceipts) {
    let rows = channels
        .iter()
        .enumerate()
        .map(|(index, channel)| ReceiptRow {
            id: format!("run-{index}"),
            turn_id: Some(format!("turn-{index}")),
            channel_id: Some(channel.to_string()),
        })
        .collect();
    (
        key.into(),
        KeyReceipts {
            name: NAME.into(),
            rows,
        },
    )
}

fn evidence(
    local: Option<u64>,
    claims: &[u64],
    receipts: Result<Vec<(String, KeyReceipts)>, String>,
) -> OwnerEvidence {
    let owners = |owner| [(NAME.to_owned(), owner)].into_iter().collect();
    OwnerEvidence {
        watchers: WatcherEvidence {
            local: owners(local),
            registered: claims.iter().map(|owner| owners(Some(*owner))).collect(),
        },
        routines: receipts.map(|keys| RoutineEvidence {
            keys: keys.into_iter().collect(),
        }),
    }
}

fn owner(proof: &OwnerEvidence) -> Option<u64> {
    proof.effective_owners(TARGET)[NAME]
}

#[test]
fn cross_runtime_watcher_agreement_preserves_local_and_self_claims() {
    assert_eq!(
        owner(&evidence(None, &[FOREIGN, FOREIGN], Ok(vec![]))),
        Some(FOREIGN)
    );
    assert_eq!(
        owner(&evidence(None, &[FOREIGN, FOREIGN + 1], Ok(vec![]))),
        None
    );
    assert_eq!(
        owner(&evidence(None, &[FOREIGN, TARGET], Ok(vec![]))),
        Some(TARGET)
    );
    assert_eq!(
        owner(&evidence(
            Some(FOREIGN),
            &[TARGET],
            Err("receipt read failed".into())
        )),
        Some(TARGET)
    );
    assert_eq!(
        owner(&evidence(
            None,
            &[FOREIGN],
            Err("receipt read failed".into())
        )),
        None
    );
    assert_eq!(
        owner(&evidence(
            None,
            &[TARGET],
            Err("receipt read failed".into())
        )),
        Some(TARGET)
    );
}

#[test]
fn cross_runtime_local_foreign_owner_conflicts_with_another_foreign_claim() {
    assert_eq!(
        owner(&evidence(Some(FOREIGN), &[FOREIGN + 1], Ok(vec![]))),
        None,
        "contradictory runtime owners cannot prove a foreign owner"
    );
    assert_eq!(
        owner(&evidence(
            Some(FOREIGN),
            &[FOREIGN],
            Ok(vec![receipt("key-a", &[FOREIGN + 1])])
        )),
        None
    );
    assert_eq!(
        owner(&evidence(
            Some(FOREIGN),
            &[FOREIGN],
            Err("receipt read failed".into())
        )),
        Some(FOREIGN)
    );
    assert_eq!(
        owner(&evidence(Some(TARGET), &[FOREIGN], Ok(vec![]))),
        Some(TARGET)
    );
}

#[test]
fn cross_runtime_receipt_distinct_keys_and_invalid_history_cannot_fill_foreign_owner() {
    let mut invalid = receipt("key-a", &[FOREIGN]);
    invalid.1.rows.push(ReceiptRow {
        id: "unstarted".into(),
        turn_id: None,
        channel_id: Some(FOREIGN.to_string()),
    });
    assert_eq!(
        owner(&evidence(None, &[FOREIGN], Ok(vec![invalid.clone()]))),
        None
    );
    assert_eq!(
        owner(&evidence(
            None,
            &[],
            Ok(vec![
                receipt("key-a", &[FOREIGN]),
                receipt("key-b", &[FOREIGN])
            ])
        )),
        None
    );
    assert_eq!(
        owner(&evidence(
            None,
            &[FOREIGN],
            Ok(vec![invalid, receipt("key-b", &[TARGET])])
        )),
        Some(TARGET)
    );
    assert_eq!(
        owner(&evidence(
            Some(FOREIGN),
            &[],
            Ok(vec![receipt("key-self", &[TARGET])])
        )),
        Some(TARGET)
    );
}

#[test]
fn cross_runtime_watcher_receipt_agreement_checks_all_historical_channels() {
    assert_eq!(
        owner(&evidence(
            None,
            &[FOREIGN, FOREIGN],
            Ok(vec![receipt("key", &[FOREIGN])])
        )),
        Some(FOREIGN)
    );
    assert_eq!(
        owner(&evidence(
            None,
            &[FOREIGN],
            Ok(vec![receipt("key", &[FOREIGN + 1])])
        )),
        None
    );
    assert_eq!(
        owner(&evidence(
            None,
            &[FOREIGN],
            Ok(vec![receipt("key", &[FOREIGN, FOREIGN + 1])])
        )),
        None
    );
    assert_eq!(
        owner(&evidence(
            None,
            &[],
            Ok(vec![receipt("key", &[FOREIGN + 1, FOREIGN])])
        )),
        Some(FOREIGN)
    );
}

async fn insert_receipt(
    pool: &sqlx::PgPool,
    id: &str,
    key: &str,
    turn: Option<&str>,
    channel: u64,
) {
    sqlx::query("INSERT INTO routine_runs (id, routine_id, status, owned_tmux_session, turn_id, result_json) VALUES ($1, 'r', 'succeeded', $2, $3, $4)")
        .bind(id).bind(key).bind(turn)
        .bind(serde_json::json!({"channel_id": channel.to_string()}))
        .execute(pool).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn registered_runtime_receipt_evidence_uses_foreign_token_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::{
        adk_session, health::HealthRegistry, make_shared_data_for_tests_with_storage,
        tmux_watcher_registry::lock_tmux_watcher_registry,
    };
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('r', 'r.js', 'r')")
        .execute(&pool)
        .await
        .unwrap();
    let registry = Arc::new(HealthRegistry::new());
    let mut caller = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let caller_mut = Arc::get_mut(&mut caller).unwrap();
    caller_mut.token_hash = "receipt-caller".into();
    caller_mut.health_registry = Arc::downgrade(&registry);
    let mut foreign = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let foreign_mut = Arc::get_mut(&mut foreign).unwrap();
    foreign_mut.provider = ProviderKind::Codex;
    foreign_mut.token_hash = "receipt-foreign".into();
    registry.register("claude".into(), caller.clone()).await;
    registry.register("codex".into(), foreign.clone()).await;
    let membership = registry.unbound_runtime_snapshot().await;
    assert_eq!(membership.entries.len(), 2);
    let watchers = {
        let guard = lock_tmux_watcher_registry();
        WatcherEvidence::capture(&guard, &caller, &membership.entries, &[NAME.into()])
    };
    assert!(
        watchers
            .registered
            .iter()
            .all(|owners| owners[NAME].is_none())
    );
    let before = OwnerEvidence::collect(watchers.clone(), &pool, &membership.entries)
        .await
        .unwrap();
    assert_eq!(owner(&before), None);
    let key =
        adk_session::build_namespaced_session_key(&foreign.token_hash, &ProviderKind::Codex, NAME);
    insert_receipt(&pool, "foreign-valid", &key, Some("turn-f"), FOREIGN).await;
    let valid = OwnerEvidence::collect(watchers.clone(), &pool, &membership.entries)
        .await
        .unwrap();
    assert_eq!(
        owner(&valid),
        Some(FOREIGN),
        "registered foreign token receipt was not queried"
    );
    assert_eq!(
        valid.routines.as_ref().unwrap().keys[&key].rows[0].id,
        "foreign-valid"
    );
    assert_ne!(before, valid);
    insert_receipt(&pool, "foreign-invalid", &key, None, FOREIGN).await;
    let invalid = OwnerEvidence::collect(watchers.clone(), &pool, &membership.entries)
        .await
        .unwrap();
    assert_eq!(
        owner(&invalid),
        None,
        "invalid historical receipt was hidden"
    );
    assert_eq!(invalid.routines.as_ref().unwrap().keys[&key].rows.len(), 2);
    insert_receipt(&pool, "foreign-self", &key, Some("turn-self"), TARGET).await;
    let self_claim = OwnerEvidence::collect(watchers.clone(), &pool, &membership.entries)
        .await
        .unwrap();
    assert_eq!(
        owner(&self_claim),
        Some(TARGET),
        "started self claim lost to invalid history"
    );
    pool.close().await;
    let failed = OwnerEvidence::collect(watchers, &pool, &membership.entries)
        .await
        .unwrap();
    assert!(failed.routines.is_err());
    assert_eq!(owner(&failed), None);
    db.drop().await;
}
