use super::*;
use crate::services::cluster::intake_router_hook::owner_record::cleanup::tests::Fixture;
use crate::services::tui_o::cutover;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) fn native(channel: u64) -> serde_json::Value {
    native_for(channel, "claude")
}
fn native_for(channel: u64, provider: &str) -> serde_json::Value {
    let owner = format!("cleanup-fixture-{channel}");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: provider.into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.clone(),
        tmux_session: owner.clone(),
        channel_id: Some(channel),
        owner_runtime_root: root.clone(),
        host: Some("mac-a".into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
    })
    .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&owner, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &nonce).unwrap();
    json!({"host":"tmux","endpoint":"mac-a","owner":owner,"root":root,"execution_nonce":nonce})
}
#[tokio::test]
async fn owner_cleanup_exact_native_rejects_successor_and_preserves_transition_pin_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_exact_native_rejects_successor_and_preserves_transition_pin_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 73369301;
    let binding = native(channel);
    let fx = Fixture::with_native(channel, Some(binding.clone())).await;
    let shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
    let row = cleanup::claim(&fx.pool, &fx.row().await)
        .await
        .unwrap()
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (entered, inside) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let task_shared = shared.clone();
    let task_calls = calls.clone();
    let task_row = row.clone();
    let task = tokio::spawn(async move {
        run_with(&task_shared, &task_row, move |_, pin| async move {
            let _pin = pin;
            task_calls.fetch_add(1, Ordering::SeqCst);
            entered.send(()).unwrap();
            released.await.unwrap();
            Response::Ended
        })
        .await
    });
    inside.await.unwrap();
    assert!(
        shared
            .acquire_session_transition(ChannelId::new(channel))
            .await
            .is_err(),
        "actual task retains the native pin while its caller is elsewhere"
    );
    drop(task);
    assert!(
        shared
            .acquire_session_transition(ChannelId::new(channel))
            .await
            .is_err(),
        "dropping the request handle must not release the end task pin"
    );
    release.send(()).unwrap();
    let next = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shared
            .session_transition_lock(ChannelId::new(channel))
            .lock_owned(),
    )
    .await
    .unwrap();
    drop(next);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    fx.wake().await;
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_state='retry' WHERE id=$1")
        .bind(row.id)
        .execute(&fx.pool)
        .await
        .unwrap();
    let row = cleanup::claim(&fx.pool, &fx.row().await)
        .await
        .unwrap()
        .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(
        binding["owner"].as_str().unwrap(),
        "spawn_nonce",
    );
    std::fs::write(marker, "ffffffffffffffffffffffffffffffff").unwrap();
    let count = calls.clone();
    let result = run_with(&shared, &row, move |_, _| async move {
        count.fetch_add(1, Ordering::SeqCst);
        Response::Ended
    })
    .await;
    assert_eq!(result, Response::IdentityChanged);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "old cleanup must never end a new incarnation"
    );
    fx.pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_transient_end_restarts_same_operation_without_new_input_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_transient_end_restarts_same_operation_without_new_input_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 73369302;
    let fx = Fixture::with_native(channel, Some(native(channel))).await;
    let mut operation = None;
    let calls = AtomicUsize::new(0);
    for response in [Response::Retry, Response::Ended] {
        fx.wake().await;
        let row = cleanup::claim(&fx.pool, &fx.row().await)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            *operation.get_or_insert(row.routing_cleanup.operation),
            row.routing_cleanup.operation
        );
        let shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
        let result = run_with(&shared, &row, |_, pin| async {
            let _pin = pin;
            calls.fetch_add(1, Ordering::SeqCst);
            response
        })
        .await;
        assert_eq!(result, response);
        assert!(cleanup::respond(&fx.pool, &row, result).await.unwrap());
        assert_eq!(
            cleanup::advance(&fx.pool, row.id).await.unwrap(),
            response == Response::Ended
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(fx.row().await.routing_cleanup_state, "ended");
    fx.pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_codex_and_herdr_refusals_do_not_gain_end_authority_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_codex_and_herdr_refusals_do_not_gain_end_authority_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 73369303;
    let binding = native_for(channel, "codex");
    let fx = Fixture::with_native_provider(channel, "codex", Some(binding.clone())).await;
    let mut shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
    Arc::get_mut(&mut shared).unwrap().provider = crate::services::provider::ProviderKind::Codex;
    let row = cleanup::claim(&fx.pool, &fx.row().await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        run_with(&shared, &row, |_, pin| async move {
            drop(pin);
            Response::Ended
        })
        .await,
        Response::Ended
    );
    fx.pool.close().await;
    drop(shared);
    drop(fx);
    let channel = 73369304;
    let binding = json!({"host":"herdr","endpoint":"local","owner":"cleanup-hosted-fixture","root":"fixture","execution_nonce":"0123456789abcdef0123456789abcdef"});
    let fx = Fixture::with_native(channel, Some(binding)).await;
    let shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
    let row = cleanup::claim(&fx.pool, &fx.row().await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        run_with(&shared, &row, |_, _| async {
            panic!("Herdr must use its existing gated terminate service")
        })
        .await,
        Response::NeedsOperator
    );
    assert!(
        cleanup::respond(&fx.pool, &row, Response::NeedsOperator)
            .await
            .unwrap()
    );
    assert_eq!(fx.row().await.routing_cleanup_state, "needs_operator");
}

#[cfg(unix)]
#[tokio::test]
async fn owner_cleanup_actual_normal_end_retries_and_retires_only_exact_launch_files_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_actual_normal_end_retries_and_retires_only_exact_launch_files_pg"
    )) {
        return;
    }
    use crate::services::provider_teardown::tests::test_support::FakeTmux;
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, absent) in [(73369305, true), (73369306, false)] {
        let binding = native(channel);
        let owner = binding["owner"].as_str().unwrap();
        let fx = Fixture::with_native(channel, Some(binding.clone())).await;
        let shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
        let fake = FakeTmux::install(owner);
        if absent {
            fake.mark_missing()
        } else {
            fake.fail_kills()
        }
        let row = cleanup::claim(&fx.pool, &fx.row().await)
            .await
            .unwrap()
            .unwrap();
        let result = run(&shared, &row).await;
        if absent {
            assert_eq!(result, Response::Ended);
            assert!(
                !fake
                    .take_calls()
                    .iter()
                    .any(|call| call.starts_with("kill-session")),
                "confirmed absence needs no signal"
            );
        } else {
            assert_eq!(result, Response::Retry, "failed normal end is not success");
            assert!(cleanup::respond(&fx.pool, &row, result).await.unwrap());
            assert!(!cleanup::advance(&fx.pool, row.id).await.unwrap());
            assert_eq!(fx.row().await.routing_cleanup_attempts, 1);
            let recovered = FakeTmux::install(owner);
            recovered.become_missing_after_kill();
            fx.wake().await;
            let retry = cleanup::claim(&fx.pool, &fx.row().await)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                retry.routing_cleanup.operation,
                row.routing_cleanup.operation
            );
            assert_eq!(run(&shared, &retry).await, Response::Ended);
            assert!(
                cleanup::respond(&fx.pool, &retry, Response::Ended)
                    .await
                    .unwrap()
            );
            assert_eq!(
                recovered
                    .take_calls()
                    .iter()
                    .filter(|call| call.starts_with("kill-session"))
                    .count(),
                1
            );
        }
        assert_eq!(
            binding_context::observe_spawn_nonce_marker(owner),
            SpawnNonceMarker::Absent,
            "confirmed end releases old launch evidence under its source pin"
        );
        fx.pool.close().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn owner_cleanup_normal_end_rechecks_source_at_physical_boundary_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_normal_end_rechecks_source_at_physical_boundary_pg"
    )) {
        return;
    }
    use crate::services::provider_teardown::tests::test_support::FakeTmux;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 73369307;
    let binding = native(channel);
    let owner = binding["owner"].as_str().unwrap();
    let fx = Fixture::with_native(channel, Some(binding.clone())).await;
    let shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
    let fake = FakeTmux::install(owner);
    fake.become_missing_after_kill();
    let row = cleanup::claim(&fx.pool, &fx.row().await)
        .await
        .unwrap()
        .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(owner, "spawn_nonce");
    let successor = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    std::fs::write(&marker, successor).unwrap();
    let pin = shared
        .session_transition_lock(ChannelId::new(channel))
        .lock_owned()
        .await;
    assert_eq!(
        normal_end(&shared, &row, owner, pin).await,
        Response::IdentityChanged
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), successor);
    assert!(
        !fake
            .take_calls()
            .iter()
            .any(|call| call.starts_with("kill-session")),
        "late old cleanup must not signal a successor"
    );
    fx.pool.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn owner_cleanup_completed_inflight_restart_archives_exact_source_before_fresh_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_completed_inflight_restart_archives_exact_source_before_fresh_pg"
    )) {
        return;
    }
    use crate::services::provider_teardown::tests::test_support::FakeTmux;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 73369308;
    let binding = native(channel);
    let owner = binding["owner"].as_str().unwrap();
    let fx = Fixture::with_native(channel, Some(binding.clone())).await;
    let shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
    let fake = FakeTmux::install(owner);
    fake.mark_missing();
    let receipt:i64=sqlx::query_scalar("SELECT r.id FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.channel_id=$1 AND d.owner_generation=0 ORDER BY d.id LIMIT 1").bind(channel.to_string()).fetch_one(&fx.pool).await.unwrap();
    let text: String = sqlx::query_scalar("SELECT user_text FROM intake_outbox WHERE id=$1")
        .bind(receipt)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let mut state = discord::inflight::InflightTurnState::new(
        crate::services::provider::ProviderKind::Claude,
        channel,
        Some("completed-source".into()),
        100,
        90100,
        9001,
        text,
        None,
        Some(owner.into()),
        None,
        None,
        0,
    );
    state.replay_receipt_id = Some(receipt);
    state.terminal_delivery_committed = true;
    let root = discord::inflight::inflight_runtime_root().unwrap();
    let path = discord::inflight::inflight_state_path(&root, &shared.provider, channel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = serde_json::to_vec_pretty(&state).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let row = cleanup::claim(&fx.pool, &fx.row().await)
        .await
        .unwrap()
        .unwrap();
    state.terminal_delivery_committed = false;
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(
        run(&shared, &row).await,
        Response::Busy,
        "uncommitted live generation is retained"
    );
    state.terminal_delivery_committed = true;
    state.replay_receipt_id = None;
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(
        run(&shared, &row).await,
        Response::NeedsOperator,
        "old format cannot fabricate receipt authority"
    );
    state.replay_receipt_id = Some(receipt);
    let mut protected = serde_json::to_value(&state).unwrap();
    protected["herdr_before_start_stop"] = json!({"unknown":"receipt"});
    std::fs::write(&path, serde_json::to_vec(&protected).unwrap()).unwrap();
    assert_eq!(
        archive_completed(&shared, &row).await,
        Response::Retry,
        "existing before-start receipt protection must survive cleanup"
    );
    std::fs::write(&path, &bytes).unwrap();
    let mut barrier = fx.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE intake_outbox IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *barrier)
        .await
        .unwrap();
    let task_shared = shared.clone();
    let task_row = row.clone();
    let archived = tokio::spawn(async move { archive_completed(&task_shared, &task_row).await });
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'SELECT r.replay_disposition%')").fetch_one(&fx.pool).await.unwrap();
            if blocked {break}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("archive must load the original snapshot before its PG authority read");
    state.save_generation += 1;
    let updated = serde_json::to_vec(&state).unwrap();
    std::fs::write(&path, &updated).unwrap();
    barrier.rollback().await.unwrap();
    assert_eq!(
        archived.await.unwrap(),
        Response::Retry,
        "a newer save generation must make archive a no-op"
    );
    assert_eq!(std::fs::read(&path).unwrap(), updated);
    assert!(!root.join("archive").exists());
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        run(&shared, &row).await,
        Response::Ended,
        "a completed source left by a restart is not a live Busy turn"
    );
    assert!(
        cleanup::respond(&fx.pool, &row, Response::Ended)
            .await
            .unwrap()
    );
    assert!(cleanup::advance(&fx.pool, row.id).await.unwrap());
    assert!(
        !path.exists(),
        "the old sidecar cannot block the next fresh generation"
    );
    let archives: Vec<_> = std::fs::read_dir(root.join("archive"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(archives.len(), 1);
    assert_eq!(
        std::fs::read(&archives[0]).unwrap(),
        bytes,
        "raw source data is retained, never cleared or rewritten"
    );
    assert!(
        !fake
            .take_calls()
            .iter()
            .any(|line| line.starts_with("kill-session"))
    );
    fx.pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_native_observer_requires_original_file_and_own_turn_end() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_native_observer_requires_original_file_and_own_turn_end"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = discord::make_shared_data_for_tests_with_storage(None);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("native.jsonl");
    let previous = "{\"type\":\"result\",\"subtype\":\"success\"}\n";
    std::fs::write(&path, previous).unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("original-native", &path)
            .unwrap();
    let mut state = discord::inflight::InflightTurnState::new(
        crate::services::provider::ProviderKind::Claude,
        73369309,
        None,
        1,
        1,
        1,
        "input".into(),
        Some("original-native".into()),
        Some("owner".into()),
        Some(path.to_string_lossy().into()),
        None,
        previous.len() as u64,
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state.tui_terminal_source_file_identity = Some((source.dev, source.ino));
    assert!(
        !native_completed(&shared, &state),
        "an earlier turn's terminator cannot close this source"
    );
    let current = "{\"type\":\"assistant\",\"uuid\":\"a\",\"apiBlockIndex\":0,\"message\":{\"id\":\"message-a\",\"content\":[{\"type\":\"text\",\"text\":\"still working\"}]}}\n";
    std::fs::write(&path, format!("{previous}{current}")).unwrap();
    assert!(
        !native_completed(&shared, &state),
        "assistant prose is not native completion"
    );
    std::fs::write(&path, format!("{previous}{current}{previous}")).unwrap();
    assert!(
        native_completed(&shared, &state),
        "original source's strict terminator completes the turn"
    );
    std::fs::rename(&path, temp.path().join("old.jsonl")).unwrap();
    std::fs::write(&path, previous).unwrap();
    assert!(
        !native_completed(&shared, &state),
        "a replacement pathname is not the admitted transcript"
    );
}
