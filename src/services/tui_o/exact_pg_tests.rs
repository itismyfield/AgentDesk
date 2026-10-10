use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[tokio::test]
async fn exact_metadata_pg_restore_and_terminal_port_stub() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let records = super::super::exact_episode::tests::fixture();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    for record in &records {
        let ack = record_episode_evidence(true, &pool, record)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((ack.record, ack.version), (record.record, 1));
        assert!(!ack.digest.is_empty());
        assert!(
            record_episode_evidence(true, &pool, record)
                .await
                .unwrap()
                .is_some()
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, before + records.len() as i64);
    let mut connection = pool.acquire().await.unwrap();
    let result = resolve_in_tx(&mut connection, records[0].episode)
        .await
        .unwrap();
    assert_eq!(result.authority(), Authority::Body);
    let target = DbTarget {
        id: 9,
        birth: 1,
        episode: records[0].episode,
    };
    assert_eq!(
        guard_terminal_in_tx(&mut connection, &target, TerminalIntent::BodyDone)
            .await
            .unwrap(),
        TerminalDisposition::Deferred
    );
    assert_eq!(
        guard_terminal_in_tx(&mut connection, &target, TerminalIntent::Delete)
            .await
            .unwrap(),
        TerminalDisposition::Deferred
    );
    let mut future = records[0].clone();
    future.version = 2;
    assert!(record_episode_evidence(true, &pool, &future).await.is_err());
    sqlx::query("UPDATE public.delivery_journal_events SET canonical_payload = jsonb_set(canonical_payload,'{version}','2') WHERE canonical_payload->>'episode'=$1").bind(records[0].episode.to_string()).execute(&mut *connection).await.unwrap();
    assert_eq!(
        resolve_in_tx(&mut connection, records[0].episode)
            .await
            .unwrap()
            .authority(),
        Authority::Pending
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exact_duplicate_pg_full_fields_and_legacy_same_key_other_attempt() {
    crate::services::discord::exact_duplicate_pg_full_fields_and_legacy_same_key_other_attempt()
        .await;
}
#[cfg(unix)]
#[tokio::test]
async fn exact_namespace_pg_old_reader_and_legacy_binding_bytes_unchanged() {
    crate::services::discord::exact_namespace_pg_old_reader_and_legacy_binding_bytes_unchanged()
        .await;
}

fn recursive_files(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(
        base: &std::path::Path,
        path: &std::path::Path,
        files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(base, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(base).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[tokio::test]
async fn exact_off_pg_statement_trace_and_files_zero() {
    if let Ok(url) = std::env::var("C2A_OFF_DATABASE") {
        let pool =
            crate::db::postgres::connect_test_pool_with_max_connections(&url, "exact off child", 1)
                .await
                .unwrap();
        let root = crate::config::runtime_root().expect("actual runtime root");
        let before = recursive_files(&root);
        let mut connection = pool.acquire().await.unwrap();
        let _pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let oid: i64 = sqlx::query_scalar(
            "SELECT oid::bigint FROM pg_database WHERE datname=current_database()",
        )
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        drop(connection);
        let admin = crate::db::postgres::connect_test_pool(
            &std::env::var("C2A_OFF_OBSERVER").unwrap(),
            "off observer",
        )
        .await
        .unwrap();
        let count = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT (xact_commit+xact_rollback)::bigint FROM pg_stat_database WHERE datid=$1::oid",
            )
            .bind(oid)
            .fetch_one(&admin)
            .await
            .unwrap()
        };
        pool.close().await;
        let flushed = || async {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let active: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datid=$1::oid")
                        .bind(oid)
                        .fetch_one(&admin)
                        .await
                        .unwrap();
                if active == 0 {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let states: Vec<(String,String,String)> = sqlx::query_as("SELECT backend_type,coalesce(state,''),query FROM pg_stat_activity WHERE datid=$1::oid")
                        .bind(oid).fetch_all(&admin).await.unwrap();
                    panic!("target backends and autovacuum workers stopped: {states:?}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let mut previous = -1;
            let mut same = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                sqlx::query("SELECT pg_stat_clear_snapshot()")
                    .execute(&admin)
                    .await
                    .unwrap();
                let value = count().await;
                same = if value == previous { same + 1 } else { 0 };
                if same >= 20 {
                    return value;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "server statistics reached stable flush"
                );
                previous = value;
            }
        };
        let _migration_flush = flushed().await;
        let vacuum_count = || async {
            let observer = crate::db::postgres::connect_test_pool_with_max_connections(
                &url,
                "vacuum observation",
                1,
            )
            .await
            .unwrap();
            let value: i64 = sqlx::query_scalar("SELECT coalesce(sum(autovacuum_count+autoanalyze_count),0)::bigint FROM pg_stat_user_tables")
                .fetch_one(&observer).await.unwrap();
            observer.close().await;
            value
        };
        let mut stable = false;
        for _ in 0..5 {
            let open = || async {
                crate::db::postgres::connect_test_pool_with_max_connections(
                    &url,
                    "off calibrated backend",
                    1,
                )
                .await
                .unwrap()
            };
            let vacuum_before = vacuum_count().await;
            let initial = flushed().await;
            let control_pool = open().await;
            sqlx::query("SELECT pg_stat_force_next_flush()")
                .execute(&control_pool)
                .await
                .unwrap();
            control_pool.close().await;
            let vacuum_after = vacuum_count().await;
            let control = flushed().await;
            if vacuum_after != vacuum_before {
                continue;
            }
            let observation_cost = control - initial;
            assert!(
                observation_cost > 0,
                "connection/flush control must be counted"
            );
            let vacuum_before = vacuum_count().await;
            let control = flushed().await;
            let positive = open().await;
            sqlx::query("SELECT 1").execute(&positive).await.unwrap();
            sqlx::query("SELECT pg_stat_force_next_flush()")
                .execute(&positive)
                .await
                .unwrap();
            positive.close().await;
            let vacuum_after = vacuum_count().await;
            let selected = flushed().await;
            if vacuum_after != vacuum_before {
                continue;
            }
            assert!(
                selected - control > observation_cost,
                "server sees SELECT control: delta={} base={}",
                selected - control,
                observation_cost
            );
            let vacuum_before = vacuum_count().await;
            let initial = flushed().await;
            let pool = open().await;
            let records = super::super::exact_episode::tests::fixture();
            assert_eq!(
                record_episode_evidence(false, &pool, &records[0])
                    .await
                    .unwrap(),
                None
            );
            let returned = tokio::task::spawn_blocking(|| {
                crate::services::tui_o::exact_submission::dispatch(|| {
                    Err::<(), _>("original OFF result".into())
                })
            })
            .await
            .unwrap();
            assert_eq!(returned, Err("original OFF result".into()));
            sqlx::query("SELECT pg_stat_force_next_flush()")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
            let vacuum_after = vacuum_count().await;
            let final_count = flushed().await;
            if vacuum_after != vacuum_before {
                continue;
            }
            assert_eq!(
                final_count - initial,
                observation_cost,
                "OFF issued server statements including spawned tasks"
            );
            stable = true;
            break;
        }
        assert!(stable, "autovacuum-free measurement window required");
        assert_eq!(
            recursive_files(&root),
            before,
            "OFF changed recursive runtime paths/content"
        );
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("VACUUM ANALYZE").execute(&pool).await.unwrap();
    sqlx::query("DO $$ DECLARE r record; BEGIN FOR r IN SELECT n.nspname,c.relname,c.reltoastrelid FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind='r' AND n.nspname='public' LOOP IF r.reltoastrelid<>0 THEN EXECUTE format('ALTER TABLE %I.%I SET (autovacuum_enabled=false, toast.autovacuum_enabled=false)',r.nspname,r.relname); ELSE EXECUTE format('ALTER TABLE %I.%I SET (autovacuum_enabled=false)',r.nspname,r.relname); END IF; END LOOP; END $$").execute(&pool).await.unwrap();
    pool.close().await;

    let observer_url = format!(
        "{}/postgres",
        crate::db::postgres::postgres_test_database_url_base().unwrap()
    );

    let runtime = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(runtime.path().join("runtime/nested")).unwrap();
    std::fs::write(runtime.path().join("runtime/existing"), "original").unwrap();
    std::fs::write(
        runtime.path().join("runtime/nested/existing"),
        "nested original",
    )
    .unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "services::tui_o::exact_pg::tests::exact_off_pg_statement_trace_and_files_zero",
            "--nocapture",
        ])
        .env("C2A_OFF_DATABASE", &db.database_url)
        .env("C2A_OFF_OBSERVER", &observer_url)
        .env("AGENTDESK_ROOT_DIR", runtime.path());
    let child = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        child.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
}

#[tokio::test]
async fn exact_ack_pg_waits_for_commit_barrier() {
    use crate::services::tui_o::exact_submission::{COMMIT_BARRIER, CommitBarrier};
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut record = super::super::exact_episode::tests::fixture().remove(0);
    record.episode = Uuid::new_v4();
    if let EpisodeEvidence::Pin(pin) = &mut record.evidence {
        pin.episode = record.episode;
    }
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    *COMMIT_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CommitBarrier {
        episode: record.episode,
        entered,
        release: released,
    });
    let p = pool.clone();
    let copy = record.clone();
    let mut task = tokio::spawn(async move { record_episode_evidence(true, &p, &copy).await });
    let pid = tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("writer reached barrier")
        .unwrap();
    let matching: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND pid=$1 AND state='idle in transaction'")
        .bind(pid).fetch_one(&pool).await.unwrap();
    assert_eq!(
        matching, 1,
        "target database/PID reached pre-COMMIT barrier"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "INSERT is not visible before COMMIT");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut task)
            .await
            .is_err(),
        "ACK before commit"
    );
    release.send(()).unwrap();
    assert!(task.await.unwrap().unwrap().is_some());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn exact_pg_process_reader_uses_only_metadata() {
    if let Ok(url) = std::env::var("C1_READER_DATABASE") {
        let pool = crate::db::postgres::connect_test_pool(&url, "exact child")
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let result = resolve_in_tx(&mut connection, Uuid::from_u128(1))
            .await
            .unwrap();
        assert_eq!(result.authority(), Authority::Body);
        assert_eq!(result.settlement(), Settlement::Settled);
        println!("C1_PROCESS_RESULT {:?}", result);
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for record in super::super::exact_episode::tests::fixture() {
        record_episode_evidence(true, &pool, &record).await.unwrap();
    }
    for id in [701, 702] {
        let record = EpisodeMetadata::new(
            Uuid::from_u128(1),
            Uuid::from_u128(id),
            EpisodeEvidence::Settled {
                effects: vec!["intake".into()],
            },
        );
        record_episode_evidence(true, &pool, &record).await.unwrap();
    }
    let with = tempfile::tempdir().unwrap();
    let without = tempfile::tempdir().unwrap();
    std::fs::create_dir(with.path().join("o")).unwrap();
    std::fs::write(with.path().join("o/ledger.jsonl"), b"local producer only").unwrap();
    let run = |root: &std::path::Path| {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "services::tui_o::exact_pg::tests::exact_pg_process_reader_uses_only_metadata",
                "--exact",
                "--nocapture",
            ])
            .env("C1_READER_DATABASE", &db.database_url)
            .env("AGENTDESK_ROOT_DIR", root)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "child failed {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .find(|s| s.starts_with("C1_PROCESS_RESULT"))
            .unwrap()
            .to_string()
    };
    let first = run(with.path());
    assert_eq!(first, run(without.path()));
    std::fs::remove_file(with.path().join("o/ledger.jsonl")).unwrap();
    assert_eq!(first, run(with.path()));
}

#[test]
fn exact_snapshot_sibling_construction_is_compile_rejected() {
    let source = include_str!("exact_pg.rs");
    let start = source
        .find("pub(crate) struct ConsistentEpisodeSnapshot")
        .unwrap();
    let end = source[start..].find("\n}").unwrap() + start + 2;
    let declaration = &source[start..end];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("privacy.rs");
    let prelude = format!("mod exact_pg {{ type EpisodeMetadata=(); {declaration} }}");
    std::fs::write(
        &path,
        format!(
            "{prelude} fn main() {{ let _: Option<exact_pg::ConsistentEpisodeSnapshot>=None; }}"
        ),
    )
    .unwrap();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let positive = std::process::Command::new(&rustc)
        .arg(&path)
        .arg("--out-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        positive.status.success(),
        "positive compile {}",
        String::from_utf8_lossy(&positive.stderr)
    );
    std::fs::write(&path,format!("{prelude} mod sibling {{ pub fn forge() {{ let _=super::exact_pg::ConsistentEpisodeSnapshot {{ records:Vec::new() }}; }} }} fn main() {{}} ")).unwrap();
    let negative = std::process::Command::new(rustc)
        .arg(&path)
        .arg("--out-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!negative.status.success());
    assert!(
        String::from_utf8_lossy(&negative.stderr)
            .contains("field `records` of struct `ConsistentEpisodeSnapshot` is private")
    );
}

#[tokio::test]
async fn exact_child_ack_precedes_terminal_seal_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for end in [30, 45] {
        let episode = Uuid::new_v4();
        let source = SourceIdentity {
            incarnation: Uuid::new_v4(),
            opener: 10,
            digest: "start-only".into(),
        };
        let pin = ExactEpisodePin {
            episode,
            owner: "owner".into(),
            execution_nonce: "execution".into(),
            turn_nonce: "turn".into(),
            inflight_identity: "inflight".into(),
            born_generation: 1,
            channel_id: "10".into(),
            expected_author: "bot".into(),
            source: Some(source.clone()),
            context: FrozenSettlementContext {
                intake: None,
                dispatch: None,
                aliases: vec![],
                required_effects: vec![],
                policy_version: 1,
            },
        };
        let child = ExactPieceRef {
            episode,
            source: source.clone(),
            native_unit: "first-row".into(),
            kind: "body".into(),
            range: (10, 20),
            plan_version: 1,
            plan_digest: "plan".into(),
            piece_index: 0,
            piece_count: 1,
            payload_digest: "body".into(),
            obligation: Uuid::new_v4(),
            attempt: Uuid::new_v4(),
        };
        let piece_frontier = FrontierWitness {
            id: Uuid::new_v4(),
            source: source.clone(),
            range: child.range,
            digest: "piece-frontier".into(),
        };
        let early: Vec<_> = [
            EpisodeEvidence::Pin(pin),
            EpisodeEvidence::Obligation {
                piece: child.clone(),
            },
            EpisodeEvidence::Attempt {
                piece: child.clone(),
                frontier: piece_frontier.clone(),
            },
        ]
        .into_iter()
        .map(|e| EpisodeMetadata::new(episode, Uuid::new_v4(), e))
        .collect();
        for record in &early {
            record_episode_evidence(true, &pool, record)
                .await
                .unwrap()
                .unwrap();
        }
        let query = "SELECT canonical_payload::text FROM public.delivery_journal_events WHERE canonical_payload->>'episode'=$1 AND canonical_payload->'evidence'->>'type' IN ('Obligation','Attempt') ORDER BY event_id";
        let before: Vec<(String,)> = sqlx::query_as(query)
            .bind(episode.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(before.len(), 2);
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(
            resolve_in_tx(&mut connection, episode)
                .await
                .unwrap()
                .authority(),
            Authority::Pending
        );
        let manifest = ExactTerminalManifest {
            source: source.clone(),
            seal: Some(TerminalSeal {
                source: source.clone(),
                terminal_identity: format!("terminal-{end}"),
                terminal_end: end,
                capture_witness: Uuid::new_v4(),
                derive_witness: Uuid::new_v4(),
            }),
            captured_through: end,
            derived_through: end,
            membership_digest: "membership".into(),
            required: vec![child.clone()],
            no_body_policy: None,
        };
        let whole = FrontierWitness {
            id: Uuid::new_v4(),
            source: source.clone(),
            range: (10, end),
            digest: "whole-frontier".into(),
        };
        let late: Vec<_> = [
            EpisodeEvidence::Transport {
                piece: child.clone(),
                receipt: DirectReceipt {
                    requested_channel: "10".into(),
                    returned_channel: "10".into(),
                    message_id: "100".into(),
                    author: "bot".into(),
                    payload_digest: "body".into(),
                },
            },
            EpisodeEvidence::Committed {
                piece: child.clone(),
                frontier: piece_frontier,
            },
            EpisodeEvidence::Manifest(manifest.clone()),
            EpisodeEvidence::WholeFrontier {
                manifest: manifest.clone(),
                pieces: vec![child],
                frontier: whole,
            },
        ]
        .into_iter()
        .map(|e| EpisodeMetadata::new(episode, Uuid::new_v4(), e))
        .collect();
        for record in &late {
            record_episode_evidence(true, &pool, record)
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            resolve_in_tx(&mut connection, episode)
                .await
                .unwrap()
                .authority(),
            Authority::Body
        );
        let after: Vec<(String,)> = sqlx::query_as(query)
            .bind(episode.to_string())
            .fetch_all(&mut *connection)
            .await
            .unwrap();
        assert_eq!(
            before, after,
            "early O/A must stay byte-identical after terminal"
        );
        for foreign in [false, true] {
            let mut invalid = manifest.clone();
            if foreign {
                invalid.seal.as_mut().unwrap().source.incarnation = Uuid::new_v4();
            } else {
                invalid.seal = None;
            }
            for record in &late {
                let mut modified = record.clone();
                match &mut modified.evidence {
                    EpisodeEvidence::Manifest(m)
                    | EpisodeEvidence::WholeFrontier { manifest: m, .. } => *m = invalid.clone(),
                    _ => continue,
                }
                sqlx::query("UPDATE public.delivery_journal_events SET canonical_payload=$1 WHERE canonical_payload->>'record'=$2").bind(serde_json::to_value(&modified).unwrap()).bind(record.record.to_string()).execute(&mut *connection).await.unwrap();
            }
            assert_eq!(
                resolve_in_tx(&mut connection, episode)
                    .await
                    .unwrap()
                    .authority(),
                Authority::Pending,
                "matching F/K bad seal foreign={foreign}"
            );
            for record in &late {
                if matches!(
                    record.evidence,
                    EpisodeEvidence::Manifest(_) | EpisodeEvidence::WholeFrontier { .. }
                ) {
                    sqlx::query("UPDATE public.delivery_journal_events SET canonical_payload=$1 WHERE canonical_payload->>'record'=$2").bind(serde_json::to_value(record).unwrap()).bind(record.record.to_string()).execute(&mut *connection).await.unwrap();
                }
            }
            assert_eq!(
                resolve_in_tx(&mut connection, episode)
                    .await
                    .unwrap()
                    .authority(),
                Authority::Body,
                "independent normal baseline restored"
            );
        }
    }
}

#[tokio::test]
async fn exact_settled_ack_names_normalized_pg_payload() {
    use sha2::{Digest, Sha256};
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut previous = None;
    for id in [1001, 1002] {
        let record = EpisodeMetadata::new(
            Uuid::from_u128(1),
            Uuid::from_u128(id),
            EpisodeEvidence::Settled {
                effects: vec!["intake".into()],
            },
        );
        let ack = record_episode_evidence(true, &pool, &record)
            .await
            .unwrap()
            .unwrap();
        let payload: serde_json::Value =
            sqlx::query_scalar("SELECT canonical_payload FROM public.delivery_journal_events")
                .fetch_one(&pool)
                .await
                .unwrap();
        let stored: EpisodeMetadata = serde_json::from_value(payload).unwrap();
        assert_eq!(ack.record, stored.record);
        assert_eq!(ack.version, stored.version);
        assert_eq!(
            ack.digest,
            format!("{:x}", Sha256::digest(serde_json::to_vec(&stored).unwrap()))
        );
        if let Some(prev) = previous {
            assert_eq!(ack, prev);
        }
        previous = Some(ack);
    }
}
