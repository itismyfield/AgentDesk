use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::InflightRestartMode;
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;

const CHANNEL: u64 = 1_479_671_301_387_250_000;
const FIELD: &str = "herdr_before_start_stop";

async fn pg() -> (TestPostgresDb, sqlx::PgPool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    (db, pool)
}

struct Fixture {
    _root: tempfile::TempDir,
    _env: crate::config::test_env::TestEnvVarGuard,
    row: InflightTurnState,
    dir: PathBuf,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let dir = inflight_runtime_root().unwrap();
        let path = inflight_state_path(&dir, &ProviderKind::Claude, CHANNEL);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            CHANNEL,
            None,
            7,
            11,
            12,
            "relay".into(),
            Some("session".into()),
            None,
            Some("/tmp/relay".into()),
            None,
            0,
        );
        row.turn_nonce = Some("receipt-boundary".into());
        row.started_at = "2000-01-01 00:00:00".into();
        row.updated_at = row.started_at.clone();
        row.born_generation = 1;
        row.save_generation = 3;
        row.full_response = "relay body".into();
        row.ensure_finalizer_turn_id();
        Self {
            _root: root,
            _env: env,
            row,
            dir,
            path,
        }
    }
    fn seed(&self, receipt: Option<serde_json::Value>) -> Vec<u8> {
        let mut raw = serde_json::to_value(&self.row).unwrap();
        if let Some(receipt) = receipt {
            raw[FIELD] = receipt;
        }
        let bytes = serde_json::to_vec_pretty(&raw).unwrap();
        fs::write(&self.path, &bytes).unwrap();
        bytes
    }
    fn bytes(&self) -> Option<Vec<u8>> {
        fs::read(&self.path).ok()
    }
    fn loaded(&self) -> InflightTurnState {
        parse_inflight_state_content(&fs::read_to_string(&self.path).unwrap()).unwrap()
    }
}
fn receipt(notice: &str) -> serde_json::Value {
    serde_json::json!({"protocol":1,"kind":"unsubmitted_before_start_stop", "turn_nonce":"receipt-boundary",
        "admitted_generation":2,"basis":"no_attempt","notice":notice})
}
fn shapes() -> Vec<serde_json::Value> {
    let mut extra = receipt("pending");
    extra["future"] = serde_json::json!({"nested":[1,null]});
    let mut wrong_type = receipt("pending");
    wrong_type["protocol"] = "1".into();
    let mut wrong_notice = receipt("pending");
    wrong_notice["notice"] = 7.into();
    vec![
        receipt("pending"),
        receipt("delivered"),
        extra,
        wrong_type,
        wrong_notice,
        serde_json::json!({"protocol":1}),
        serde_json::Value::Null,
        serde_json::json!([1, "future"]),
        serde_json::json!("future"),
        serde_json::json!(7),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_unknown_receipt_json_shape_preserved_pg() {
    let f = Fixture::new();
    let (_db, pool) = pg().await;
    for shape in shapes() {
        f.seed(Some(shape.clone()));
        let loaded = f.loaded();
        let roundtrip = serde_json::to_value(&loaded).unwrap();
        assert_eq!(roundtrip.get(FIELD), Some(&shape), "raw shape {shape}");
        assert_eq!(
            loaded.herdr_before_start_stop.as_ref().unwrap().supported(),
            shape == receipt("pending") || shape == receipt("delivered"),
            "strict V1 {shape}"
        );
        save_inflight_state(&f.row).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&f.bytes().unwrap()).unwrap();
        assert_eq!(saved.get(FIELD), Some(&shape), "carry {shape}");
    }
    let raw = serde_json::to_value(&f.row).unwrap();
    let mut with_null = raw.clone();
    with_null[FIELD] = serde_json::Value::Null;
    let mut encoded =
        serde_json::to_value(serde_json::from_value::<InflightTurnState>(with_null).unwrap())
            .unwrap();
    assert_eq!(
        encoded.as_object_mut().unwrap().remove(FIELD),
        Some(serde_json::Value::Null)
    );
    assert_eq!(
        serde_json::to_vec(&encoded).unwrap(),
        serde_json::to_vec(&raw).unwrap(),
        "other Option fields byte-identical"
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_generic_store_cannot_mint_or_raise_receipt_pg() {
    let f = Fixture::new();
    let (_db, pool) = pg().await;
    f.seed(Some(receipt("pending")));
    let pending = f.loaded();
    let mut value = serde_json::to_value(&pending).unwrap();
    value[FIELD]["notice"] = "delivered".into();
    let raised: InflightTurnState = serde_json::from_value(value).unwrap();
    save_inflight_state(&raised).unwrap();
    assert_eq!(
        serde_json::to_value(f.loaded()).unwrap()[FIELD],
        receipt("pending"),
        "generic raise forbidden"
    );
    let mut introductions = Vec::new();
    for create in 0..3 {
        fs::remove_file(&f.path).unwrap();
        let result = match create {
            0 => save_inflight_state(&pending)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            1 => save_inflight_state_create_new(&pending).map_err(|e| e.to_string()),
            _ => save_inflight_state_if_absent(&pending)
                .map(|_| ())
                .map_err(|e| e.to_string()),
        };
        eprintln!("generic introduction {create}: {result:?}");
        introductions.push((create, result.is_err() && !f.path.exists()));
        f.seed(None);
    }
    assert!(
        introductions.iter().all(|(_, refused)| *refused),
        "all three introductions evaluated: {introductions:?}"
    );
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    let admitted = f
        .loaded()
        .admit_before_start_receipt(
            "receipt-boundary",
            crate::services::provider::herdr_before_start::CloseBasis::NoAttempt,
            2,
        )
        .await
        .unwrap();
    let acked = admitted
        .ack_before_start_notice(&Ok(
            crate::services::discord::formatting::ReplaceLongMessageOutcome::EditedOriginal,
        ))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(acked).unwrap()[FIELD],
        receipt("delivered"),
        "authorized admission/ACK positive"
    );
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_scoped_invalidation_preserves_receipt_pg() {
    let mut f = Fixture::new();
    f.row.restart_generation = Some(1);
    let (_db, pool) = pg().await;
    for shape in shapes() {
        let before = f.seed(Some(shape));
        let loaded = f.loaded();
        assert!(
            !invalidate_stale_generation_scoped(Some(&pool), &ProviderKind::Claude, 2, &loaded)
                .await
                .unwrap()
        );
        assert_eq!(f.bytes().unwrap(), before);
    }
    let before = f.seed(Some(receipt("pending")));
    let snapshot = f.loaded();
    fs::remove_file(&f.path).unwrap();
    assert!(
        !invalidate_stale_generation_scoped(Some(&pool), &ProviderKind::Claude, 2, &snapshot)
            .await
            .unwrap(),
        "snapshot protection before any fresh I/O"
    );
    f.seed(None);
    let snapshot = f.loaded();
    fs::write(&f.path, &before).unwrap();
    assert!(
        !invalidate_stale_generation_scoped(Some(&pool), &ProviderKind::Claude, 2, &snapshot)
            .await
            .unwrap(),
        "fresh receipt recorded after snapshot"
    );
    assert_eq!(f.bytes(), Some(before));
    f.seed(None);
    assert!(
        invalidate_stale_generation_scoped(Some(&pool), &ProviderKind::Claude, 2, &f.loaded())
            .await
            .unwrap(),
        "receipt-free scoped invalidation positive"
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_receipt_survives_all_legacy_deletes_pg() {
    let f = Fixture::new();
    let (_db, pool) = pg().await;
    for shape in shapes() {
        for clear in 0..5 {
            let before = f.seed(Some(shape.clone()));
            let row = f.loaded();
            let id = InflightTurnIdentity::from_state(&row);
            match clear {
                0 => {
                    delete_inflight_state_file(&ProviderKind::Claude, CHANNEL);
                }
                1 => {
                    clear_inflight_state(&ProviderKind::Claude, CHANNEL);
                }
                2 => {
                    clear_inflight_state_if_matches(
                        &ProviderKind::Claude,
                        CHANNEL,
                        row.user_msg_id,
                    );
                }
                3 => {
                    clear_inflight_state_if_matches_identity_generation(
                        &ProviderKind::Claude,
                        CHANNEL,
                        &id,
                        row.effective_finalizer_turn_id(),
                        &row.updated_at,
                        row.save_generation,
                    );
                }
                _ => {
                    request_inflight_abandon_if_matches(
                        &ProviderKind::Claude,
                        CHANNEL,
                        row.user_msg_id,
                        "fixture",
                    );
                }
            }
            assert_eq!(f.bytes(), Some(before), "clear {clear}, shape {shape}");
        }
    }
    let before = f.seed(Some(receipt("pending")));
    assert!(removal::stale_removal_reason(&f.loaded(), INFLIGHT_MAX_AGE_SECS + 1, 9).is_none());
    std::fs::File::open(&f.path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::now()
                - std::time::Duration::from_secs(INFLIGHT_MAX_AGE_SECS + 100),
        ))
        .unwrap();
    assert_eq!(
        removal::receipt_boundary_reap(&f.dir, &ProviderKind::Claude),
        0
    );
    assert_eq!(f.bytes(), Some(before));
    f.seed(None);
    assert!(
        delete_inflight_state_file(&ProviderKind::Claude, CHANNEL),
        "receipt-free delete positive"
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_generic_inflight_regression_off_pg() {
    let _clock = FixedClock::new();
    let f = Fixture::new();
    let (_db, pool) = pg().await;
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let mut outputs = serde_json::Map::new();
    let mut refused = Vec::new();
    for op in [
        "save",
        "create",
        "if_absent",
        "identity_anchor",
        "identity_rebind",
        "rewind",
        "anchor_repost",
        "budget",
        "clear",
        "abandon",
        "offset",
        "restart",
    ] {
        f.seed(None);
        let row = f.loaded();
        if op == "create" || op == "if_absent" {
            fs::remove_file(&f.path).unwrap();
        }
        let result = writer(&f, &row, op);
        eprintln!("receipt-free writer {op}: {result}");
        if !matches!(
            result.as_str(),
            "Ok(())" | "Ok(true)" | "Saved" | "Cleared" | "true"
        ) {
            refused.push((op, result.clone()));
        }
        outputs.insert(op.into(), serde_json::json!({"result":result,"bytes":f.bytes().map(|b| String::from_utf8(b).unwrap())}));
    }
    assert!(
        refused.is_empty(),
        "all writers evaluated; receipt-free refusals: {refused:?}"
    );
    let outputs = serde_json::Value::Object(outputs);
    assert_eq!(
        outputs,
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../../tests/fixtures/discord_inflight/receipt_free_writer_baseline.json"
        ))
        .unwrap(),
        "receipt-free golden from pre-fix execution"
    );
    if let Ok(path) = std::env::var("ADK_B3B2_COMPARE_BASELINE") {
        let expected: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            outputs, expected,
            "all receipt-free writer results and complete bytes match pre-fix"
        );
    }
    if let Ok(path) = std::env::var("ADK_B3B2_RECORD_BASELINE") {
        fs::write(path, serde_json::to_vec_pretty(&outputs).unwrap()).unwrap();
    }
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_rebind_gc_preserves_receipt_under_lock_pg() {
    let mut f = Fixture::new();
    let (_db, pool) = pg().await;
    f.row.rebind_origin = true;
    f.row.turn_source = TurnSource::ExternalAdopted;
    f.row.user_msg_id = 0;
    f.row.current_msg_id = 0;
    f.row.full_response.clear();
    f.row.turn_start_offset = Some(0);
    f.row.rebind_origin_created_at_unix = Some(now_unix() - 3600);
    f.row.rebind_origin_birth_generation = Some(1);
    for watcher in [false, true] {
        f.row.set_relay_owner_kind(if watcher {
            RelayOwnerKind::Watcher
        } else {
            RelayOwnerKind::None
        });
        for shape in shapes() {
            let before = f.seed(Some(shape));
            let snapshot = f.row.clone();
            removal::receipt_test_events::take();
            let outcome = if watcher {
                rebind_reap::reap_dead_watcher_rebind_origin_locked_in_root(
                    &f.dir,
                    &ProviderKind::Claude,
                    &snapshot,
                    9,
                )
            } else {
                reap_abandoned_rebind_origin_locked_in_root(
                    &f.dir,
                    &ProviderKind::Claude,
                    &snapshot,
                    9,
                )
            };
            assert_eq!(
                outcome,
                rebind_reap::RebindReapOutcome::Skipped,
                "fresh receipt after receipt-free snapshot, watcher={watcher}"
            );
            assert_eq!(f.bytes(), Some(before));
            assert!(
                removal::receipt_test_events::take().is_empty(),
                "unlink/log 0"
            );
        }
    }
    pool.close().await;
}

fn writer(f: &Fixture, row: &InflightTurnState, op: &str) -> String {
    let id = InflightTurnIdentity::from_state(row);
    match op {
            "save" => format!("{:?}", save_inflight_state(row)),
            "create" => format!("{:?}", save_inflight_state_create_new(row)),
            "if_absent" => format!("{:?}", save_inflight_state_if_absent(row)),
            "identity_anchor" => format!("{:?}", bind_recovery_anchor_if_matches_identity(&ProviderKind::Claude, CHANNEL, &id, row.turn_start_offset, row.current_msg_id, Some(row.current_msg_len), 13, 1, None, None)),
            "identity_rebind" => format!("{:?}", save_store::identity_gate::save_existing_inflight_rebind_adoption_if_matches_identity_in_root(&f.dir, &row, &id, row.turn_start_offset)),
            "rewind" => format!("{:?}", save_inflight_delivery_rewind_if_matches_identity(&row, store::InflightDeliveryRewindReason::TerminalErrorReset)),
            "anchor_repost" => format!("{:?}", anchor_repost::mark_anchor_reposted_if_matches_identity_in_root(&f.dir, &ProviderKind::Claude, CHANNEL, &id, row.turn_start_offset)),
            "budget" => format!("{:?}", budget::bump_recovery_relay_attempts_if_matches_identity_in_root(&f.dir, &ProviderKind::Claude, CHANNEL, &id, row.turn_start_offset)),
            "clear" => format!("{:?}", clear_inflight_state_if_matches(&ProviderKind::Claude, CHANNEL, row.user_msg_id)),
            "abandon" => format!("{:?}", request_inflight_abandon_if_matches(&ProviderKind::Claude, CHANNEL, row.user_msg_id, "fixture")),
            "offset" => format!("{:?}", refresh_inflight_last_offset_if_matches_identity(&ProviderKind::Claude, CHANNEL, &id, row.turn_start_offset, "/tmp/relay", Some(row.current_msg_id), 9, RelayOwnerKind::None)),
            _ => format!("{:?}", set_inflight_restart_mode_under_lock(&f.path, InflightRestartMode::DrainRestart)),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_writer_matrix_carries_raw_receipt_pg() {
    let f = Fixture::new();
    let (_db, pool) = pg().await;
    for shape in shapes() {
        for op in [
            "save",
            "identity_anchor",
            "identity_rebind",
            "rewind",
            "anchor_repost",
            "budget",
            "offset",
            "restart",
        ] {
            f.seed(Some(shape.clone()));
            let row = f.loaded();
            let result = writer(&f, &row, op);
            assert!(
                matches!(result.as_str(), "Ok(())" | "Ok(true)" | "Saved" | "true"),
                "{op}: {result}"
            );
            let saved: serde_json::Value = serde_json::from_slice(&f.bytes().unwrap()).unwrap();
            assert_eq!(saved[FIELD], shape, "writer {op} preserved subtree");
        }
    }
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_clear_entrypoint_matrix_pg() {
    let mut f = Fixture::new();
    let (_db, pool) = pg().await;
    let original = f.row.clone();
    for op in [
        "identity",
        "returning",
        "nonce",
        "captured",
        "snapshot",
        "reconcile",
        "rebind",
        "lifecycle",
        "archive",
        "zero",
        "abandon_zero",
        "abandon_episode",
        "delivery",
        "tmux",
        "operator",
        "native",
    ] {
        for protected in [true, false] {
            f.row = original.clone();
            if matches!(op, "zero" | "abandon_zero") {
                f.row.user_msg_id = 0;
            }
            if matches!(op, "rebind" | "lifecycle") {
                f.row.rebind_origin = true;
            }
            if op == "reconcile" {
                f.row.born_generation = 0;
            }
            if op == "tmux" {
                f.row.tmux_session_name = Some("fixture-no-live".into());
            }
            if op == "native" {
                f.row.tui_terminal_kind =
                    Some(crate::services::agent_protocol::NativeTerminalKind::Aborted);
                f.row.terminal_delivery_committed = true;
            }
            let before = f.seed(protected.then(|| receipt("pending")));
            let row = f.loaded();
            guard_reads();
            let id = InflightTurnIdentity::from_state(&row);
            let provider = &ProviderKind::Claude;
            let result = match op {
                "identity" => clear_inflight_state_if_matches_identity(provider, CHANNEL, &id),
                "returning" => {
                    clear_inflight_state_if_matches_identity_returning_row(provider, CHANNEL, &id).0
                }
                "nonce" => clear_inflight_state_if_matches_identity_turn_nonce(
                    provider,
                    CHANNEL,
                    &id,
                    row.turn_nonce.as_deref(),
                ),
                "captured" => clear_store::clear_inflight_state_for_captured_episode(
                    provider,
                    CHANNEL,
                    &id,
                    row.turn_nonce.as_deref(),
                ),
                "snapshot" => clear_inflight_state_for_snapshot(provider, &row),
                "reconcile" => {
                    match clear_store::clear_inflight_state_for_reconcile(provider, &row) {
                        ReconcileClearOutcome::Delegated(outcome) => outcome,
                        _ => GuardedClearOutcome::UserMsgMismatch,
                    }
                }
                "rebind" => clear_rebind_origin_inflight_state_if_matches_identity(
                    provider,
                    CHANNEL,
                    &id,
                    row.turn_nonce.as_deref(),
                ),
                "lifecycle" => {
                    clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence(
                        provider,
                        CHANNEL,
                        &id,
                        &row.updated_at,
                        row.save_generation,
                    )
                }
                "archive" => clear_store::archive_inflight_state_if_matches_identity_generation(
                    provider,
                    CHANNEL,
                    &id,
                    &row.updated_at,
                    row.save_generation,
                    "fixture",
                ),
                "zero" => clear_inflight_state_if_matches_zero_owned(provider, CHANNEL),
                "abandon_zero" => {
                    request_inflight_abandon_if_matches_zero_owned(provider, CHANNEL, "fixture")
                }
                "abandon_episode" => clear_store::request_inflight_abandon_for_captured_episode(
                    provider,
                    CHANNEL,
                    &InflightEpisodePin::from_state(&row),
                    "fixture",
                ),
                "delivery" => {
                    clear_store::clear_inflight_state_if_matches_identity_after_delivery_in_root(
                        &f.dir,
                        provider,
                        CHANNEL,
                        &id,
                        row.turn_start_offset,
                        &row.full_response,
                        row.full_response.len(),
                        row.last_offset,
                    )
                    .0
                }
                "tmux" => clear_store::clear_inflight_state_if_matches_tmux_response_in_root(
                    &f.dir,
                    provider,
                    CHANNEL,
                    "fixture-no-live",
                    &row.full_response,
                ),
                "operator" => {
                    let lock = lock_inflight_state_path(&f.path).unwrap();
                    clear_store::operator_disposition_remove_pinned(
                        &lock,
                        &InflightEpisodePin::from_state(&row),
                    )
                    .0
                }
                _ => clear_store::clear_admitted_restart_terminal(
                    provider,
                    &row,
                    "receipt-boundary",
                    9,
                ),
            };
            assert!(guard_reads() > 0, "raw guard really read {op}");
            assert_eq!(
                result == GuardedClearOutcome::Cleared,
                !protected,
                "entrypoint {op} protected={protected}: {result:?}"
            );
            if protected {
                assert_eq!(f.bytes(), Some(before), "entrypoint {op}");
            } else {
                assert!(!f.path.exists(), "positive {op}");
            }
        }
    }
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_unparseable_receipt_row_preserved_pg() {
    let f = Fixture::new();
    let (_db, pool) = pg().await;
    let mut value = serde_json::to_value(&f.row).unwrap();
    value["channel_id"] = "unreadable".into();
    value[FIELD] = receipt("pending");
    for bytes in [
        serde_json::to_vec_pretty(&value).unwrap(),
        b"{\"herdr_before_start_stop\":null,broken".to_vec(),
        b"{\"herdr_before_start_stop\":null,\xff".to_vec(),
    ] {
        fs::write(&f.path, &bytes).unwrap();
        assert!(save_inflight_state(&f.row).is_err());
        assert!(!delete_inflight_state_file(&ProviderKind::Claude, CHANNEL));
        clear_inflight_state(&ProviderKind::Claude, CHANNEL);
        assert_eq!(
            removal::receipt_boundary_reap(&f.dir, &ProviderKind::Claude),
            0
        );
        assert_eq!(f.bytes(), Some(bytes));
    }
    value.as_object_mut().unwrap().remove(FIELD);
    for bytes in [
        serde_json::to_vec(&value).unwrap(),
        b"{\"user_text\":\"herdr_before_start_stop\",broken".to_vec(),
        b"{\"nested\":{\"herdr_before_start_stop\":null},broken".to_vec(),
    ] {
        fs::write(&f.path, bytes).unwrap();
        save_inflight_state(&f.row).unwrap();
    }
    assert!(
        f.path.exists(),
        "legacy malformed receipt-free save is still lenient"
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_generation_and_loader_protect_receipt_pg() {
    let mut f = Fixture::new();
    let (_db, pool) = pg().await;
    f.row.restart_generation = Some(1);
    let before = f.seed(Some(receipt("pending")));
    assert!(invalidate_stale_generation_in_root(&f.dir, &ProviderKind::Claude, 9).is_empty());
    assert_eq!(f.bytes(), Some(before));
    f.seed(None);
    assert_eq!(
        invalidate_stale_generation_in_root(&f.dir, &ProviderKind::Claude, 9).len(),
        1
    );
    let mut value = serde_json::to_value(&f.row).unwrap();
    value["provider"] = "gemini".into();
    value[FIELD] = receipt("pending");
    let before = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(&f.path, &before).unwrap();
    assert_eq!(
        removal::receipt_boundary_reap_total(&f.dir, &ProviderKind::Claude),
        0
    );
    assert_eq!(f.bytes(), Some(before));
    value.as_object_mut().unwrap().remove(FIELD);
    fs::write(&f.path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        removal::receipt_boundary_reap_total(&f.dir, &ProviderKind::Claude),
        1
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_receipt_commit_wins_concurrent_clear_pg() {
    let f = Fixture::new();
    let (_db, pool) = pg().await;
    f.seed(None);
    let lock = lock_inflight_state_path(&f.path).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let a = barrier.clone();
    let delete = std::thread::spawn(move || {
        a.wait();
        delete_inflight_state_file(&ProviderKind::Claude, CHANNEL)
    });
    let b = barrier.clone();
    let clear = std::thread::spawn(move || {
        b.wait();
        clear_inflight_state(&ProviderKind::Claude, CHANNEL)
    });
    barrier.wait();
    let before = f.seed(Some(receipt("pending")));
    drop(lock);
    assert!(
        !delete.join().unwrap(),
        "delete re-read under canonical lock"
    );
    assert!(!clear.join().unwrap(), "clear re-read under canonical lock");
    assert_eq!(f.bytes(), Some(before));
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_receipt_free_reaper_results_and_logs_pg() {
    let mut f = Fixture::new();
    let (_db, pool) = pg().await;
    f.row.rebind_origin = true;
    f.row.turn_source = TurnSource::ExternalAdopted;
    f.row.user_msg_id = 0;
    f.row.current_msg_id = 0;
    f.row.full_response.clear();
    f.row.turn_start_offset = Some(0);
    f.row.rebind_origin_created_at_unix = Some(now_unix() - 3600);
    f.row.rebind_origin_birth_generation = Some(1);
    let mut outputs = Vec::new();
    for watcher in [false, true] {
        f.row.set_relay_owner_kind(if watcher {
            RelayOwnerKind::Watcher
        } else {
            RelayOwnerKind::None
        });
        f.seed(None);
        let row = f.loaded();
        removal::receipt_test_events::take();
        let result = if watcher {
            rebind_reap::reap_dead_watcher_rebind_origin_locked_in_root(
                &f.dir,
                &ProviderKind::Claude,
                &row,
                9,
            )
        } else {
            reap_abandoned_rebind_origin_locked_in_root(&f.dir, &ProviderKind::Claude, &row, 9)
        };
        let logs = removal::receipt_test_events::take();
        assert_eq!(result, rebind_reap::RebindReapOutcome::Reaped);
        assert!(!f.path.exists());
        assert_eq!(logs.len(), 1);
        outputs.push(serde_json::json!({"watcher":watcher,"result":format!("{result:?}"),"exists":f.path.exists(),"logs":logs}));
    }
    let outputs = serde_json::to_value(outputs).unwrap();
    assert_eq!(
        outputs,
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../../tests/fixtures/discord_inflight/receipt_free_reaper_baseline.json"
        ))
        .unwrap()
    );
    for (env, record) in [
        ("ADK_B3B2_RECORD_BASELINE", true),
        ("ADK_B3B2_COMPARE_BASELINE", false),
    ] {
        if let Ok(path) = std::env::var(env) {
            let path = Path::new(&path).with_file_name("receipt-free-reaper-before.json");
            if record {
                fs::write(path, serde_json::to_vec_pretty(&outputs).unwrap()).unwrap();
            } else {
                assert_eq!(
                    outputs,
                    serde_json::from_slice::<serde_json::Value>(&fs::read(path).unwrap()).unwrap()
                );
            }
        }
    }
    pool.close().await;
}

thread_local! {
    static FIXED_NOW: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}
pub(super) fn fixed_now() -> Option<&'static str> {
    FIXED_NOW.get()
}
struct FixedClock(Option<&'static str>);
impl FixedClock {
    fn new() -> Self {
        Self(FIXED_NOW.replace(Some("2000-01-01 00:00:00")))
    }
}
impl Drop for FixedClock {
    fn drop(&mut self) {
        FIXED_NOW.set(self.0);
    }
}

thread_local! {
    static GUARD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(super) fn note_guard_read() {
    GUARD_READS.set(GUARD_READS.get() + 1);
}
fn guard_reads() -> usize {
    GUARD_READS.replace(0)
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_clear_and_boot_recovery_observe_fresh_receipt_pg() {
    let mut f = Fixture::new();
    let (_db, pool) = pg().await;
    f.row.provider = "gemini".into();
    f.seed(None);
    let lock = lock_inflight_state_path(&f.path).unwrap();
    let (entered, observed) = std::sync::mpsc::sync_channel(0);
    let (release, released) = std::sync::mpsc::sync_channel(0);
    let root = f.dir.clone();
    let recovery = std::thread::spawn(move || {
        removal::receipt_test_events::reap_after_snapshot(&root, entered, released)
    });
    observed
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let clear = std::thread::spawn(move || clear_inflight_state(&ProviderKind::Claude, CHANNEL));
    let before = f.seed(Some(receipt("pending")));
    release.send(()).unwrap();
    drop(lock);
    assert_eq!(
        recovery.join().unwrap(),
        (0, 1),
        "fresh receipt is explicitly kept after a foreign snapshot"
    );
    assert!(!clear.join().unwrap());
    assert_eq!(f.bytes(), Some(before));
    pool.close().await;
}
