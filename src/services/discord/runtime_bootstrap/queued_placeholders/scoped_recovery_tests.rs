use super::*;
use crate::services::discord::{inflight, make_shared_data_for_tests, queue_dispatch};
use crate::services::turn_orchestrator::{self as queues, InterventionMode};
use std::fs;
use std::path::Path;

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: serenity::UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "restored request".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn row(channel: u64) -> inflight::InflightTurnState {
    inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        11,
        12,
        "original request".into(),
        None,
        None,
        None,
        None,
        0,
    )
}

fn write_row(state: &inflight::InflightTurnState) -> std::path::PathBuf {
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        state.channel_id,
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(state).unwrap()).unwrap();
    path
}

fn fingerprint(path: &Path) -> Option<(Vec<u8>, std::time::SystemTime)> {
    fs::read(path)
        .ok()
        .zip(fs::metadata(path).ok()?.modified().ok())
}

#[tokio::test]
async fn s3act_b1_scoped_readers_preserve_foreign_files_and_mailboxes() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (target, other) = (ChannelId::new(5340101), ChannelId::new(5340102));
    let (provider, token) = (&shared.provider, &shared.token_hash);
    queues::save_channel_queue(provider, token, target, &[item(11)], Some(99)).unwrap();
    queues::save_channel_pending_dispatch_marker(provider, token, target, &item(12), Some(99))
        .unwrap();
    let tmp = runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(provider.as_str())
        .join(token)
        .join(format!(".{}.json.stale.tmp", other.get()));
    fs::write(&tmp, "foreign tmp").unwrap();
    fs::File::open(&tmp)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    let placeholder = runtime_store::discord_queued_placeholders_root()
        .unwrap()
        .join(provider.as_str())
        .join(token)
        .join(format!("{}.json", other.get()));
    fs::create_dir_all(placeholder.parent().unwrap()).unwrap();
    fs::write(&placeholder, "malformed foreign placeholder").unwrap();
    let legacy = write_row(&row(other.get()));
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&legacy).unwrap()).unwrap();
    json.as_object_mut().unwrap().remove("finalizer_turn_id");
    fs::write(&legacy, serde_json::to_vec(&json).unwrap()).unwrap();
    let before = [&tmp, &placeholder, &legacy].map(|path| fingerprint(path));
    let (queued, role) =
        queues::load_channel_pending_queue_checked(provider, token, target).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(role, Some(ChannelId::new(99)));
    assert!(
        queues::load_channel_pending_dispatch_marker_checked(provider, token, target)
            .unwrap()
            .is_some()
    );
    assert!(
        queued_placeholders_store::load_channel_queued_placeholders_checked(
            provider, token, target
        )
        .unwrap()
        .is_empty()
    );
    let probe = inflight::load_inflight_probe_scoped(provider, target.get());
    assert!(
        probe.complete && probe.states.is_empty(),
        "normal empty target must complete"
    );
    assert_eq!(
        [&tmp, &placeholder, &legacy].map(|path| fingerprint(path)),
        before,
        "scope must precede foreign cleanup and old-format backfill"
    );
    assert!(
        shared.mailboxes.snapshot_all().await.is_empty(),
        "readers install nothing"
    );
    assert!(shared.queued.queued_placeholders.is_empty());
}

#[test]
fn s3act_b1_checked_reads_distinguish_absence_from_corruption() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let (provider, target, token) = (ProviderKind::Claude, ChannelId::new(5340111), "scope");
    assert!(
        queues::load_channel_pending_queue_checked(&provider, token, target)
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        queues::load_channel_pending_dispatch_marker_checked(&provider, token, target)
            .unwrap()
            .is_none()
    );
    let dir = runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(provider.as_str())
        .join(token);
    fs::create_dir_all(&dir).unwrap();
    for suffix in ["json", "dispatch"] {
        fs::write(dir.join(format!("{}.{}", target.get(), suffix)), "bad").unwrap();
    }
    assert!(queues::load_channel_pending_queue_checked(&provider, token, target).is_err());
    assert!(
        queues::load_channel_pending_dispatch_marker_checked(&provider, token, target).is_err()
    );
    let path = runtime_store::discord_queued_placeholders_root()
        .unwrap()
        .join(provider.as_str())
        .join(token)
        .join(format!("{}.json", target.get()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for bytes in [
        "bad",
        "[{\"user_message_id\":0,\"placeholder_message_id\":5}]",
    ] {
        fs::write(&path, bytes).unwrap();
        assert!(
            queued_placeholders_store::load_channel_queued_placeholders_checked(
                &provider, token, target
            )
            .is_err(),
            "malformed scoped placeholder must not read as empty"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            bytes,
            "failed read must not delete evidence"
        );
    }
}

#[test]
fn s3act_b1_scoped_probe_requires_complete_read_and_backfill() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let (provider, channel) = (ProviderKind::Claude, 5340121);
    assert!(inflight::load_inflight_probe_scoped(&provider, channel).complete);
    let path = write_row(&row(channel));
    fs::write(&path, "malformed").unwrap();
    assert!(
        !inflight::load_inflight_probe_scoped(&provider, channel).complete,
        "unreadable target must not become a complete empty scan"
    );
    let channel = channel + 100;
    let state = row(channel);
    let path = write_row(&state);
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json.as_object_mut().unwrap().remove("finalizer_turn_id");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    fs::create_dir(path.with_extension("json.lock")).unwrap();
    assert!(
        !inflight::load_inflight_probe_scoped(&provider, channel).complete,
        "failed required old-format backfill must remain incomplete"
    );
    fs::remove_dir(path.with_extension("json.lock")).unwrap();
    let loaded = inflight::load_inflight_probe_scoped(&provider, channel);
    assert!(loaded.complete);
    assert_eq!(loaded.states.len(), 1);
    assert!(serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap()
        ["finalizer_turn_id"].as_u64().is_some_and(|id| id > 0));
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(
        !inflight::load_inflight_probe_scoped(&provider, channel).complete,
        "entry read failure is not normal absence"
    );
    fs::remove_dir(&path).unwrap();
    let mut wrong = row(channel);
    wrong.channel_id += 1;
    fs::write(&path, serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(
        !inflight::load_inflight_probe_scoped(&provider, channel).complete,
        "filename and row channel must agree before installation"
    );
}

#[test]
fn s3act_b1_protected_probe_does_not_backfill() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let channel = 5340122;
    let path = write_row(&row(channel));
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json.as_object_mut().unwrap().remove("finalizer_turn_id");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let before = fingerprint(&path);
    let gate = crate::services::discord::input_runtime::fence::Gate::protect(
        ProviderKind::Claude,
        channel,
    )
    .unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let loaded = inflight::load_inflight_probe_scoped(&ProviderKind::Claude, channel);
    assert!(
        !loaded.complete && loaded.states.is_empty(),
        "protected is held, not restored"
    );
    assert_eq!(fingerprint(&path), before);
    assert!(
        !path.with_extension("json.lock").exists(),
        "fence precedes row lock/backfill"
    );
}

#[cfg(unix)]
#[test]
fn s3act_b1_old_format_backfill_requires_save_ack() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let channel = 5340123;
    let path = write_row(&row(channel));
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json.as_object_mut().unwrap().remove("finalizer_turn_id");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    fs::write(path.with_extension("json.lock"), "").unwrap();
    let before = fingerprint(&path);
    let parent = path.parent().unwrap();
    let permissions = fs::metadata(parent).unwrap().permissions();
    fs::set_permissions(parent, fs::Permissions::from_mode(0o500)).unwrap();
    let loaded = inflight::load_inflight_probe_scoped(&ProviderKind::Claude, channel);
    fs::set_permissions(parent, permissions).unwrap();
    assert!(
        !loaded.complete,
        "failed backfill save must not acknowledge old-format installation"
    );
    assert_eq!(fingerprint(&path), before);
}

#[tokio::test]
async fn s3act_b1_install_rolls_back_on_required_persistence_failure() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(5340131);
    let ctx = queue_dispatch::persistence_context(&shared, &shared.provider, channel);
    assert!(
        shared
            .mailbox(channel)
            .enqueue(item(11), ctx)
            .await
            .enqueued
    );
    let path = runtime_store::discord_queued_placeholders_root()
        .unwrap()
        .join(shared.provider.as_str())
        .join(&shared.token_hash)
        .join(format!("{}.json", channel.get()));
    fs::create_dir_all(&path).unwrap();
    let loaded = [((channel, MessageId::new(11)), MessageId::new(91))].into();
    assert!(
        install_channel_queued_placeholders_checked(&shared, channel, loaded)
            .await
            .is_err(),
        "failed persistence must not acknowledge installation"
    );
    assert!(
        shared.queued.queued_placeholders.is_empty(),
        "failed installation rolls map back"
    );
    fs::remove_dir(&path).unwrap();
    let loaded = [
        ((channel, MessageId::new(11)), MessageId::new(91)),
        ((channel, MessageId::new(99)), MessageId::new(92)),
    ]
    .into();
    let stale = install_channel_queued_placeholders_checked(&shared, channel, loaded)
        .await
        .unwrap();
    assert_eq!(
        stale,
        vec![(channel, MessageId::new(99), MessageId::new(92))]
    );
    let disk = queued_placeholders_store::load_channel_queued_placeholders_checked(
        &shared.provider,
        &shared.token_hash,
        channel,
    )
    .unwrap();
    assert_eq!(
        disk,
        [((channel, MessageId::new(11)), MessageId::new(91))].into()
    );
}

#[tokio::test]
async fn s3act_b1_scoped_invalidation_preserves_replay_and_foreign_rows_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let provider = ProviderKind::Claude;
    let generation = runtime_store::process_generation().max(2);
    let mut state = row(5340141);
    state.restart_generation = Some(generation - 1);
    state.replay_hold_reasons.push("canonical hold".into());
    let held_path = write_row(&state);
    let mut foreign = row(5340142);
    foreign.restart_generation = Some(generation - 1);
    let foreign_path = write_row(&foreign);
    let before = [&held_path, &foreign_path].map(|path| fingerprint(path));
    assert!(
        !inflight::invalidate_stale_generation_scoped(Some(&pool), &provider, generation, &state)
            .await
            .unwrap(),
        "held row is retained before generation cleanup"
    );
    assert_eq!(
        [&held_path, &foreign_path].map(|path| fingerprint(path)),
        before
    );
    state.channel_id += 10;
    state.replay_hold_reasons.clear();
    state.replay_receipt_id = Some(99999);
    let projected = write_row(&state);
    assert!(
        !inflight::invalidate_stale_generation_scoped(Some(&pool), &provider, generation, &state)
            .await
            .unwrap()
    );
    assert!(
        projected.exists(),
        "unverified projected receipt must survive"
    );
    state.channel_id += 10;
    state.replay_receipt_id = None;
    let canonical = write_row(&state);
    sqlx::query("INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id, channel_id,
        user_msg_id, request_owner_id, user_text, turn_kind, agent_id, provider, status,
        replay_only, replay_disposition, replay_source_message_ids, replay_episode_nonce,
        replay_owner_incarnation, replay_request_hash, replay_request_key, replay_hold_reason, replay_preserved)
        VALUES ('mini','gw',$1,'11','7','original request','foreground','agent','claude','unknown',
        TRUE,'withheld',$2,'episode','incarnation','hash',$3,'held','{}')")
        .bind(state.channel_id.to_string()).bind(vec!["11".to_string()])
        .bind(uuid::Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
    assert!(
        !inflight::invalidate_stale_generation_scoped(Some(&pool), &provider, generation, &state)
            .await
            .unwrap()
    );
    assert!(
        canonical.exists(),
        "canonical source hold protects an unprojected row"
    );
    state.channel_id += 10;
    let ordinary = write_row(&state);
    assert!(
        inflight::invalidate_stale_generation_scoped(Some(&pool), &provider, generation, &state)
            .await
            .unwrap()
    );
    assert!(
        !ordinary.exists(),
        "verified ordinary stale generation is invalidated"
    );
    assert_eq!(fingerprint(&foreign_path), before[1]);
    state.channel_id += 10;
    let unknown = write_row(&state);
    assert!(
        inflight::invalidate_stale_generation_scoped(None, &provider, generation, &state)
            .await
            .is_err()
    );
    assert!(
        unknown.exists(),
        "receipt query unavailable must never delete"
    );
    state.channel_id += 10;
    let replaced = write_row(&state);
    let mut successor = state.clone();
    successor.user_msg_id += 1;
    fs::write(&replaced, serde_json::to_vec(&successor).unwrap()).unwrap();
    let successor_bytes = fingerprint(&replaced);
    assert!(
        inflight::invalidate_stale_generation_scoped(Some(&pool), &provider, generation, &state)
            .await
            .is_err(),
        "snapshot cannot invalidate a replaced row under the lock"
    );
    assert_eq!(fingerprint(&replaced), successor_bytes);
    pool.close().await;
    db.drop().await;
}
