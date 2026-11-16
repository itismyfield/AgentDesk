use super::*;
use crate::services::cluster::intake_router_hook::owner_record::output::Scope;
use crate::services::tui_o::shadow::{SourceId, capture::file_identity};

pub(crate) fn row(channel: u64) -> Value {
    json!({"provider":"claude","channel_id":channel,"request_owner_user_id":100,"user_msg_id":90001,
        "turn_nonce":"original-row","started_at":"2026-11-15 03:00:00","born_generation":12,
        "runtime_kind":"claude_tui","host_locator":{"host_kind":"herdr","host_session_id":"original","pane":"%2"},
        "full_response":"retained answer","response_sent_offset":0})
}
pub(crate) fn seed(root: &Path, row: Value, evidence: Value) -> PathBuf {
    let episode = sha(json!([
        row["provider"],
        row["channel_id"],
        row["request_owner_user_id"],
        row["user_msg_id"],
        row["turn_nonce"],
        row.get("born_generation").unwrap_or(&json!(0)),
        row["started_at"]
    ])
    .to_string()
    .as_bytes());
    let channel = row["channel_id"].as_u64().unwrap();
    let record = json!({"version":1,"episode_key":episode,"provider":"claude","channel_id":channel,
        "row_json":row.to_string(),"evidence":evidence,"recovery_notice":"restarted","disposition":"retained"});
    let encoded = record.to_string();
    let directory = directory(root, ShadowProvider::Claude);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!(
        "{channel}-{episode}-{}.json",
        sha(encoded.as_bytes())
    ));
    std::fs::write(&path, encoded).unwrap();
    path
}
fn plans(root: &Path, channel: u64) -> Vec<Plan> {
    read(root, ShadowProvider::Claude, channel, None).unwrap()
}
fn native(root: &Path, row: &Value, text: &str) -> Value {
    let path = root.join("native.jsonl");
    let line = json!({"type":"assistant","apiBlockIndex":0,"message":{"id":"old-native","content":[{"type":"text","text":text}]}});
    let encoded = format!("{line}\n");
    std::fs::write(&path, &encoded).unwrap();
    let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
    let source = SourceId {
        session_id: "original-session".into(),
        path,
        dev,
        ino,
    };
    json!({"source":source,"execution_nonce":row["turn_nonce"],"outcome":{"kind":"terminal","start":0,"end":encoded.len(),"result":text}})
}
#[test]
fn rowless_debt_enters_a_plan_after_boot() {
    let root = tempfile::tempdir().unwrap();
    let original = row(7651301);
    let path = seed(root.path(), original.clone(), json!({"source":null}));
    let bytes = std::fs::read(&path).unwrap();
    let plan = plans(root.path(), 7651301)
        .pop()
        .expect("rowless debt is read at boot");
    let document = plan.registration(42).unwrap();
    assert_eq!(document["effects"][0]["payload"], "retained answer");
    let snapshot = document["snapshots"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(snapshot["bytes"].as_str().unwrap().as_bytes(), bytes);
    assert_eq!(
        serde_json::from_str::<Value>(snapshot["row_json"].as_str().unwrap()).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "reader never rewrites C1"
    );
}
#[test]
fn old_snapshot_input_merge_preserves_original_provenance() {
    let root = tempfile::tempdir().unwrap();
    let mut old = row(7651302);
    old.as_object_mut().unwrap().remove("born_generation");
    old["future_original_evidence"] = json!({"opaque":"preserve exactly"});
    seed(root.path(), old.clone(), json!({"source":null}));
    let mut later = old.clone();
    later["born_generation"] = json!(0);
    later["source_message_ids"] = json!([90001, 90002]);
    later["input_origin_ids"] = json!(["machine:original"]);
    seed(root.path(), later, json!({"source":null}));
    let plan = plans(root.path(), 7651302).pop().unwrap();
    let document = plan.registration(42).unwrap();
    assert_eq!(document["snapshots"].as_object().unwrap().len(), 2);
    let rows: Vec<Value> = document["snapshots"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| serde_json::from_str(v["row_json"].as_str().unwrap()).unwrap())
        .collect();
    assert!(
        rows.contains(&old),
        "old raw provenance is not replaced by a current input"
    );
    assert!(
        rows.iter()
            .any(|r| r["input_origin_ids"] == json!(["machine:original"]))
    );
    assert!(
        rows.iter()
            .all(|r| r["future_original_evidence"] == json!({"opaque":"preserve exactly"}))
    );
}
#[test]
fn ambiguous_snapshot_and_native_overlap_remains_retained() {
    let root = tempfile::tempdir().unwrap();
    let old = row(7651303);
    seed(root.path(), old.clone(), json!({"source":null}));
    let mut later = old.clone();
    later["full_response"] = json!("retained answer extended");
    let path = seed(root.path(), later, json!({"source":null}));
    assert!(plans(root.path(), 7651303)[0].registration(42).is_err());
    std::fs::remove_file(path).unwrap();
    let evidence = native(root.path(), &old, "unrelated native answer");
    seed(root.path(), old, evidence);
    assert!(plans(root.path(), 7651303)[0].registration(42).is_err());
}
#[test]
fn a_new_source_with_matching_text_cannot_supply_old_input_coverage() {
    let root = tempfile::tempdir().unwrap();
    let original = row(7651304);
    let evidence = native(root.path(), &original, "retained answer");
    seed(root.path(), original, evidence);
    assert!(plans(root.path(), 7651304)[0].registration(42).is_err());
}
#[test]
fn unreadable_replaced_or_repeated_native_coordinates_stay_retained() {
    let root = tempfile::tempdir().unwrap();
    let old = row(7651305);
    let mut evidence = native(root.path(), &old, "retained answer");
    evidence["source"]["ino"] = json!(7651);
    seed(root.path(), old, evidence);
    assert!(plans(root.path(), 7651305)[0].registration(42).is_err());
}
#[test]
fn boot_wake_only_visits_valid_debt_channels_once() {
    let root = tempfile::tempdir().unwrap();
    let old = row(7651306);
    seed(root.path(), old.clone(), json!({"source":null}));
    let mut later = old;
    later["source_message_ids"] = json!([90001]);
    seed(root.path(), later, json!({"source":null}));
    std::fs::write(
        directory(root.path(), ShadowProvider::Claude).join("malformed.json"),
        "{}",
    )
    .unwrap();
    let mut woke = Vec::new();
    wake_channels(root.path(), ShadowProvider::Claude, |channel| {
        woke.push(channel)
    });
    assert_eq!(woke, vec![7651306]);
    wake_channels(root.path(), ShadowProvider::Codex, |channel| {
        panic!("wrong provider {channel}")
    });
}
#[test]
fn unknown_kind_wrong_provider_and_changed_snapshot_are_not_adopted() {
    let root = tempfile::tempdir().unwrap();
    let mut unknown = row(7651307);
    unknown["runtime_kind"] = json!("future");
    let path = seed(root.path(), unknown, json!({"source":null}));
    assert!(
        read(root.path(), ShadowProvider::Claude, 7651307, None)
            .unwrap()
            .is_empty()
    );
    let bytes = std::fs::read(&path).unwrap();
    assert!(path.exists());
    let mut encoded: Value = serde_json::from_slice(&bytes).unwrap();
    encoded["provider"] = json!("codex");
    std::fs::write(&path, encoded.to_string()).unwrap();
    assert!(
        read(root.path(), ShadowProvider::Claude, 7651307, None)
            .unwrap()
            .is_empty()
    );
    assert!(path.exists(), "unknown or corrupted evidence stays on disk");
    let root = tempfile::tempdir().unwrap();
    let mut original = row(7651307);
    original.as_object_mut().unwrap().remove("host_locator");
    original["tmux_session_name"] = json!("AgentDesk-claude-original");
    let path = seed(root.path(), original, json!({"source":null}));
    assert!(
        read(root.path(), ShadowProvider::Claude, 7651308, None)
            .unwrap()
            .is_empty()
    );
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut changed: Value = serde_json::from_str(record["row_json"].as_str().unwrap()).unwrap();
    changed["turn_nonce"] = json!("another-original-episode");
    record["row_json"] = json!(changed.to_string());
    let encoded = record.to_string();
    std::fs::remove_file(&path).unwrap();
    let invalid = path.with_file_name(format!(
        "7651307-{}-{}.json",
        record["episode_key"].as_str().unwrap(),
        sha(encoded.as_bytes())
    ));
    std::fs::write(&invalid, &encoded).unwrap();
    assert!(
        read(root.path(), ShadowProvider::Claude, 7651307, None)
            .unwrap()
            .is_empty(),
        "a valid file digest never repairs a wrong original episode"
    );
    assert_eq!(std::fs::read_to_string(invalid).unwrap(), encoded);
}
#[tokio::test]
async fn shared_coverage_registration_has_one_effect_and_no_grant_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = tempfile::tempdir().unwrap();
    let old = row(7651308);
    seed(root.path(), old.clone(), json!({"source":null}));
    let plan = plans(root.path(), 7651308).pop().unwrap();
    let scope = Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    assert_eq!(scope.register_herdr_body_debt(&plan, 42).await.unwrap(), 1);
    assert_eq!(
        scope.register_herdr_body_debt(&plan, 42).await.unwrap(),
        0,
        "a reboot has no second send budget"
    );
    let mut later = old;
    later["input_origin_ids"] = json!(["routine:original"]);
    seed(root.path(), later, json!({"source":null}));
    let plan = plans(root.path(), 7651308).pop().unwrap();
    assert_eq!(scope.register_herdr_body_debt(&plan, 42).await.unwrap(), 0);
    let (effects,grants,runs):(i64,i64,i64)=sqlx::query_as("SELECT (SELECT COUNT(*) FROM o_herdr_debt_effects),(SELECT COUNT(*) FROM o_herdr_debt_grants),(SELECT COUNT(*) FROM o_herdr_debt_runs)").fetch_one(&pool).await.unwrap();
    assert_eq!((effects, grants, runs), (1, 0, 0));
    let (snapshots, provenance): (Value, Value) =
        sqlx::query_as("SELECT snapshots,provenance FROM o_herdr_debt_effects")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(snapshots.as_array().unwrap().len(), 2);
    assert_eq!(provenance["snapshots"].as_object().unwrap().len(), 2);
    assert!(
        scope.register_herdr_body_debt(&plan, 43).await.is_err(),
        "sender change is not a fresh effect"
    );
    pool.close().await;
    drop(db);
}
#[tokio::test]
async fn concurrent_coverage_and_conflicting_shared_plans_cannot_mint_another_budget_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = tempfile::tempdir().unwrap();
    let old = row(7651309);
    let path = seed(root.path(), old.clone(), json!({"source":null}));
    let plan = plans(root.path(), 7651309).pop().unwrap();
    let scope = Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    let (a, b) = tokio::join!(
        scope.register_herdr_body_debt(&plan, 42),
        scope.register_herdr_body_debt(&plan, 42)
    );
    assert_eq!(a.unwrap() + b.unwrap(), 1);
    std::fs::remove_file(path).unwrap();
    let mut changed = old;
    changed["full_response"] = json!("changed body");
    seed(root.path(), changed, json!({"source":null}));
    let conflicting = plans(root.path(), 7651309).pop().unwrap();
    assert!(
        scope
            .register_herdr_body_debt(&conflicting, 42)
            .await
            .is_err()
    );
    let foreign = Scope {
        pool: pool.clone(),
        provider: "codex".into(),
    };
    assert!(foreign.register_herdr_body_debt(&plan, 42).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM o_herdr_debt_effects")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    drop(db);
}

#[tokio::test]
async fn original_native_source_range_and_admission_are_propagated_pg() {
    use crate::services::cluster::intake_router_hook::owner_record::output::tests as original_tests;
    use crate::services::tui_o::shadow::CapturedRecord;
    use crate::services::tui_o::store::ledger::LedgerEntry;
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    let (db, pool, origin) = original_tests::fixture("7651310").await;
    original_tests::revoke(&pool, &origin).await;
    let root = tempfile::tempdir().unwrap();
    let mut original = row(7651310);
    original["intake_outbox_id"] = serde_json::to_value(&origin).unwrap()["delivery"].clone();
    let evidence = native(root.path(), &original, "retained answer");
    let source: SourceId = serde_json::from_value(evidence["source"].clone()).unwrap();
    let bytes = std::fs::read(&source.path).unwrap();
    let record = CapturedRecord {
        start: 0,
        end: bytes.len() as u64,
        line: bytes[..bytes.len() - 1].to_vec(),
    };
    let mut deriver = super::super::pieces::UnitDeriver::new(7651310, ShadowProvider::Claude);
    let piece = deriver
        .derive(&record)
        .into_iter()
        .find_map(|d| match d {
            super::super::pieces::Derived::Piece(p) => Some(p),
            _ => None,
        })
        .unwrap();
    let disk = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    let era = disk
        .begin_fixture_era(&[7651310], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![],
                initial_anchor: 0,
                build_digest: "fixture".into(),
                at: chrono::Utc::now(),
                policy_parent: None,
            })
        })
        .unwrap();
    let mut store = disk.open_channel(&era, 7651310).unwrap().unwrap();
    store
        .append_ledger(LedgerEntry::OutputOrigin {
            serial: 0,
            artifact: origin.seal(&piece.payload),
        })
        .unwrap();
    store
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: piece.unit_key,
            piece_index: piece.index,
            payload: piece.payload,
            anchor_id: 0,
            epoch: 1,
            local_retry: None,
            notice_origin: None,
        })
        .unwrap();
    seed(root.path(), original.clone(), evidence.clone());
    let plans = read(
        root.path(),
        ShadowProvider::Claude,
        7651310,
        Some(store.ledger()),
    )
    .unwrap();
    let document = plans[0].registration(42).unwrap();
    let alias = &document["native"][0];
    assert_eq!(alias["coordinate"]["source"], evidence["source"]);
    assert_eq!(alias["coordinate"]["end"], evidence["outcome"]["end"]);
    assert_eq!(
        alias["output_origin"]["origin"]["delivery"],
        original["intake_outbox_id"]
    );
    let mut rewritten: Value = serde_json::from_slice(&record.line).unwrap();
    rewritten["message"]["id"] = json!("other-input-same-text");
    let replacement = format!("{rewritten}\n");
    std::fs::write(&source.path, &replacement).unwrap();
    let mut newer = evidence;
    newer["outcome"]["end"] = json!(replacement.len());
    let path = directory(root.path(), ShadowProvider::Claude);
    for entry in std::fs::read_dir(&path).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    seed(root.path(), original, newer);
    assert!(
        read(
            root.path(),
            ShadowProvider::Claude,
            7651310,
            Some(store.ledger())
        )
        .unwrap()[0]
            .registration(42)
            .is_err(),
        "new native IDs cannot borrow the old text's admission"
    );
    pool.close().await;
    drop(db);
}

#[tokio::test]
async fn split_effects_share_one_durable_manifest_and_keep_every_original_snapshot_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let scope = Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    let root = tempfile::tempdir().unwrap();
    let mut original = row(7651311);
    original["full_response"] = json!("original retained paragraph\n".repeat(1000));
    seed(root.path(), original.clone(), json!({"source":null}));
    let first = plans(root.path(), 7651311).pop().unwrap();
    let pieces = scope.register_herdr_body_debt(&first, 42).await.unwrap();
    assert!(pieces > 1, "real body splitter produced multiple effects");
    original["source_message_ids"] = json!([90001, 90002]);
    seed(root.path(), original, json!({"source":null}));
    let merged = plans(root.path(), 7651311).pop().unwrap();
    assert_eq!(
        scope.register_herdr_body_debt(&merged, 42).await.unwrap(),
        0
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM o_herdr_debt_effects WHERE provenance ? 'snapshots'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 1,
        "raw original body is stored once, not once per piece"
    );
    let rows:Vec<(String,String,String,String,Value,Value)>=sqlx::query_as("SELECT provider,channel_id,episode,input_key,provenance,snapshots FROM o_herdr_debt_effects").fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), pieces);
    for (provider, channel, episode, input, reference, hashes) in rows {
        let manifest:Value=sqlx::query_scalar("SELECT provenance FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND input_key=$4 AND coverage=$5")
            .bind(provider).bind(channel).bind(episode).bind(input).bind(reference["manifest_coverage"].as_str().unwrap()).fetch_one(&pool).await.unwrap();
        assert_eq!(manifest["snapshots"].as_object().unwrap().len(), 2);
        assert_eq!(hashes.as_array().unwrap().len(), 2);
        assert!(
            manifest["snapshots"]
                .as_object()
                .unwrap()
                .values()
                .all(|snapshot| snapshot["bytes"].as_str().is_some())
        );
    }
    let grants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM o_herdr_debt_grants")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(grants, 0);
    pool.close().await;
    drop(db);
}

#[test]
fn an_unreadable_snapshot_keeps_only_its_episode_without_reordering_other_plans() {
    let root = tempfile::tempdir().unwrap();
    let channel = 7651314;
    for nonce in ["first-original", "second-original"] {
        let mut good = row(channel);
        good["turn_nonce"] = json!(nonce);
        seed(root.path(), good, json!({"source":null}));
    }
    let expected: Vec<_> = plans(root.path(), channel)
        .iter()
        .map(|plan| plan.registration(42).unwrap())
        .collect();
    let mut unknown = row(channel);
    unknown["turn_nonce"] = json!("unknown-original");
    unknown["runtime_kind"] = json!("future-kind");
    let invalid = seed(root.path(), unknown, json!({"source":null}));
    let bytes = std::fs::read(&invalid).unwrap();
    let actual: Vec<_> = plans(root.path(), channel)
        .iter()
        .map(|plan| plan.registration(42).unwrap())
        .collect();
    assert_eq!(
        actual, expected,
        "a held row does not poison other original episodes or order"
    );
    assert_eq!(std::fs::read(&invalid).unwrap(), bytes);
    let first = &expected[0];
    let digest = first["snapshots"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap();
    let path = directory(root.path(), ShadowProvider::Claude).join(format!(
        "{channel}-{}-{digest}.json",
        first["episode"].as_str().unwrap()
    ));
    let mut corrupt = std::fs::read(&path).unwrap();
    corrupt.push(b'\n');
    let copy = path.with_file_name(format!(
        "{channel}-{}-{}.json",
        first["episode"].as_str().unwrap(),
        "0".repeat(64)
    ));
    std::fs::write(&copy, &corrupt).unwrap();
    let actual = plans(root.path(), channel);
    assert_eq!(actual.len(), 2);
    assert!(
        actual[0].registration(42).is_err(),
        "unreadable evidence retains its own episode"
    );
    assert_eq!(actual[1].registration(42).unwrap(), expected[1]);
    assert_eq!(std::fs::read(copy).unwrap(), corrupt);
}

async fn split_native_coverage(text: &str, channel: u64) {
    use crate::services::cluster::intake_router_hook::owner_record::output::tests as original_tests;
    use crate::services::tui_o::shadow::CapturedRecord;
    use crate::services::tui_o::store::ledger::LedgerEntry;
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    let (db, pool, origin) = original_tests::fixture(&channel.to_string()).await;
    original_tests::revoke(&pool, &origin).await;
    let root = tempfile::tempdir().unwrap();
    let mut original = row(channel);
    original["full_response"] = json!(text);
    original["intake_outbox_id"] = serde_json::to_value(&origin).unwrap()["delivery"].clone();
    let evidence = native(root.path(), &original, text);
    let source: SourceId = serde_json::from_value(evidence["source"].clone()).unwrap();
    let bytes = std::fs::read(&source.path).unwrap();
    let record = CapturedRecord {
        start: 0,
        end: bytes.len() as u64,
        line: bytes[..bytes.len() - 1].to_vec(),
    };
    let mut deriver = super::super::pieces::UnitDeriver::new(channel, ShadowProvider::Claude);
    let pieces: Vec<_> = deriver
        .derive(&record)
        .into_iter()
        .filter_map(|d| match d {
            super::super::pieces::Derived::Piece(piece) => Some(piece),
            _ => None,
        })
        .collect();
    assert!(
        pieces.len() > 1,
        "the fixture must exercise the real splitter"
    );
    let disk = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    let era = disk
        .begin_fixture_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![],
                initial_anchor: 0,
                build_digest: "fixture".into(),
                at: chrono::Utc::now(),
                policy_parent: None,
            })
        })
        .unwrap();
    let mut store = disk.open_channel(&era, channel).unwrap().unwrap();
    for (serial, piece) in pieces.iter().enumerate() {
        store
            .append_ledger(LedgerEntry::OutputOrigin {
                serial: serial as u64,
                artifact: origin.seal(&piece.payload),
            })
            .unwrap();
        store
            .append_ledger(LedgerEntry::Prepared {
                serial: serial as u64,
                unit_key: piece.unit_key.clone(),
                piece_index: piece.index,
                payload: piece.payload.clone(),
                anchor_id: 0,
                epoch: 1,
                local_retry: None,
                notice_origin: None,
            })
            .unwrap();
    }
    let path = seed(root.path(), original, evidence);
    let saved = std::fs::read(&path).unwrap();
    let plans = read(
        root.path(),
        ShadowProvider::Claude,
        channel,
        Some(store.ledger()),
    )
    .unwrap();
    let plan = &plans[0];
    let document = plan
        .registration(42)
        .expect("equal ordered split payloads are original native coverage");
    let aliases = document["native"].as_array().unwrap();
    let effects = document["effects"].as_array().unwrap();
    assert_eq!(aliases.len(), pieces.len());
    assert_eq!(
        aliases.iter().map(|a| &a["payload"]).collect::<Vec<_>>(),
        effects.iter().map(|e| &e["payload"]).collect::<Vec<_>>()
    );
    assert!(
        aliases
            .iter()
            .all(|a| a["output_origin"]["origin"]["delivery"]
                == serde_json::to_value(&origin).unwrap()["delivery"])
    );
    let scope = Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    assert_eq!(
        scope.register_herdr_body_debt(plan, 42).await.unwrap(),
        pieces.len()
    );
    assert_eq!(scope.register_herdr_body_debt(plan, 42).await.unwrap(), 0);
    let grants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM o_herdr_debt_grants")
        .fetch_one(&pool)
        .await
        .unwrap();
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM o_herdr_debt_runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((grants, runs), (0, 0));
    assert_eq!(std::fs::read(path).unwrap(), saved);
    pool.close().await;
    drop(db);
}

#[tokio::test]
async fn newline_split_payloads_preserve_original_native_aliases_without_grants_pg() {
    split_native_coverage(
        &format!("{}\n{}", "a".repeat(1000), "b".repeat(1000)),
        7651315,
    )
    .await;
}

#[tokio::test]
async fn fenced_split_payloads_preserve_original_native_aliases_without_grants_pg() {
    split_native_coverage(
        &format!(
            "```rust\n{}\n```",
            "let original = 123456789;\n".repeat(200)
        ),
        7651316,
    )
    .await;
}
