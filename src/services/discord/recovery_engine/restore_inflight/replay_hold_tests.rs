//! Actual restart scan: a durable hold preserves the episode while delivering its saved debt.

use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, nameless_turn, seed, shared_on,
};
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
use poise::serenity_prelude::ChannelId;

pub(super) fn apply_read_error_mutant(
    state: &mut inflight::InflightTurnState,
    error: &sqlx::Error,
) {
    if std::env::var("ADK_REPLAY_FENCE_MUTANT").ok().as_deref() == Some("restore-read-error") {
        state
            .replay_hold_reasons
            .push(format!("replay source disposition unreadable: {error}"));
    }
}

async fn fixture() -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    if std::env::var("ADK_REPLAY_FENCE_MUTANT").ok().as_deref() != Some("restore-read-error") {
        crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    }
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&pool)
        .await
        .expect("actual PostgreSQL must answer");
    eprintln!("replay restore real-PG fixture: {version}");
    (db, pool)
}

async fn receipt(
    pool: &sqlx::PgPool,
    state: &inflight::InflightTurnState,
    disposition: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id, channel_id,
             user_msg_id, request_owner_id, user_text, turn_kind, agent_id, provider, status,
             replay_only, replay_disposition, replay_source_message_ids, replay_episode_nonce,
             replay_hold_reason, replay_preserved)
         VALUES ('worker', 'leader', $1, $2, '7', $3, 'standard', 'agent', 'claude', 'unknown',
             TRUE, $4, $5, $6, 'activity observed', $7) RETURNING id",
    )
    .bind(state.channel_id.to_string())
    .bind(state.user_msg_id.to_string())
    .bind(&state.user_text)
    .bind(disposition)
    .bind(vec![state.user_msg_id.to_string()])
    .bind(&state.turn_nonce)
    .bind(serde_json::json!({"body": state.full_response}))
    .fetch_one(pool)
    .await
    .expect("seed durable receipt on actual postgres")
}

fn row(channel: u64, name: String) -> inflight::InflightTurnState {
    let mut state = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        channel + 2,
        "original user request".into(),
        Some("original-provider-session".into()),
        Some(name),
        None,
        None,
        321,
    );
    state.full_response =
        "already published prefix\ncurrent anchor prefix\nsaved partial result".into();
    state.response_sent_offset = "already published prefix\n".len();
    state.current_msg_len = "current anchor prefix\n".len();
    state.streaming_rollover_frozen_msg_ids = vec![channel + 3, channel + 4];
    state.any_tool_used = true;
    state.born_generation = 0;
    state
}

#[tokio::test]
async fn replay_hold_restore_delivers_debt_without_finalizing_or_readopting_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let mut originals = Vec::new();
    let mut probes = Vec::new();
    let transcripts = tempfile::tempdir().unwrap();
    for (n, (disposition, projected)) in [
        ("withheld", true),
        ("started_unclassified", true),
        ("startup_failed_no_effect", true),
        ("withheld", false),
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 1_479_671_301_387_170_000 + n as u64;
        let name = provider.build_tmux_session_name(&format!("replay-held-{n}"));
        let key = channel_key(&shared, &name);
        seed(&pool, &key, &name, channel, Stored::Legacy).await;
        probes.push((
            InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent),
            InjectedPresenceGuard::set(HostSessionRef::tmux(&name), HostPresence::Missing),
        ));
        let mut state = row(channel, name);
        let output = transcripts.path().join(format!("{n}.jsonl"));
        std::fs::write(
            &output,
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"saved partial result\"}\n",
        )
        .unwrap();
        state.output_path = Some(output.display().to_string());
        let mut receipt_state = state.clone();
        if !projected {
            receipt_state.user_msg_id = channel + 11;
            state.source_message_ids.push(receipt_state.user_msg_id);
        }
        let id = receipt(&pool, &receipt_state, disposition).await;
        state.replay_receipt_id = projected.then_some(id);
        inflight::save_inflight_state_create_new(&state).unwrap();
        originals.push((state, id, disposition));
    }
    let discord = o_cut_recorder::start(originals[0].0.channel_id).await;
    for (original, ..) in &originals {
        crate::services::discord::http::edit_channel_message(
            &discord.http,
            ChannelId::new(original.channel_id),
            MessageId::new(original.current_msg_id),
            "current anchor prefix\n",
        )
        .await
        .unwrap();
    }

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    for (original, id, disposition) in &originals {
        let state = inflight::load_inflight_state(&provider, original.channel_id)
            .expect("held episode must remain after body delivery");
        assert!(state.replay_rerun_blocked());
        assert_eq!(state.replay_receipt_id, Some(*id));
        assert_eq!(state.full_response, original.full_response);
        assert_eq!(state.user_text, original.user_text);
        assert_eq!(state.session_id, original.session_id);
        assert_eq!(state.output_path, original.output_path);
        assert_eq!(state.tmux_session_name, original.tmux_session_name);
        assert_eq!(state.input_fifo_path, original.input_fifo_path);
        assert_eq!(state.turn_nonce, original.turn_nonce);
        assert_eq!(
            state.streaming_rollover_frozen_msg_ids,
            original.streaming_rollover_frozen_msg_ids
        );
        assert_eq!(state.last_offset, original.last_offset);
        assert!(state.any_tool_used);
        assert!(
            state.terminal_delivery_completed(),
            "saved suffix should reach Discord"
        );
        assert_eq!(state.response_sent_offset, state.full_response.len());
        assert!(
            !shared
                .core
                .lock()
                .await
                .sessions
                .contains_key(&ChannelId::new(original.channel_id))
        );
        assert!(
            !shared
                .tmux_watchers
                .contains_key(&ChannelId::new(original.channel_id))
        );
        assert!(
            super::super::tmux_probe::reader_trace::events(
                original.tmux_session_name.as_deref().unwrap()
            )
            .is_empty()
        );
        let durable: (String, String, serde_json::Value) = sqlx::query_as(
            "SELECT replay_disposition, user_text, replay_preserved FROM intake_outbox WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(durable.0, *disposition);
        assert_eq!(durable.1, original.user_text);
        assert_eq!(durable.2["body"], original.full_response);
    }
    let delivered = discord.contents();
    assert_eq!(
        delivered
            .iter()
            .filter(|body| body.contains("saved partial result"))
            .count(),
        originals.len()
    );
    assert!(
        delivered
            .iter()
            .filter(|body| body.contains("saved partial result"))
            .all(|body| body.contains("current anchor prefix"))
    );
    assert!(
        delivered
            .iter()
            .all(|body| !body.contains("already published prefix"))
    );
    let sends = delivered.len();
    restore_inflight_turns(&discord.http, &shared, &provider).await;
    assert_eq!(
        discord.contents().len(),
        sends,
        "delivery receipt prevents another body send"
    );
    drop(probes);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_missing_receipt_keeps_request_and_frozen_state_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let mut original = row(1_479_671_301_387_171_000, "missing-receipt".into());
    original.replay_receipt_id = Some(i64::MAX);
    original.response_sent_offset = original.full_response.len();
    inflight::save_inflight_state_create_new(&original).unwrap();
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &provider,
        original.channel_id,
    );
    let before = std::fs::read(&path).unwrap();
    let discord = o_cut_recorder::start(original.channel_id).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    let state = inflight::load_inflight_state(&provider, original.channel_id)
        .expect("missing authority must keep debt");
    assert!(state.replay_hold_reasons.is_empty());
    assert_eq!(state.replay_receipt_id, original.replay_receipt_id);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(state.full_response, original.full_response);
    assert_eq!(state.user_text, original.user_text);
    assert_eq!(state.session_id, original.session_id);
    assert_eq!(
        state.streaming_rollover_frozen_msg_ids,
        original.streaming_rollover_frozen_msg_ids
    );
    assert_eq!(state.response_sent_offset, original.response_sent_offset);
    assert!(!state.terminal_delivery_completed());
    assert!(discord.contents().is_empty());
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_failed_delivery_retains_the_original_debt_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let mut original = row(1_479_671_301_387_172_000, "failed-held-delivery".into());
    original.replay_receipt_id = Some(receipt(&pool, &original, "withheld").await);
    inflight::save_inflight_state_create_new(&original).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let app = axum::Router::new().fallback(axum::routing::any(|| async {
        (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"message":"Missing Permissions","code":50013})),
        )
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    restore_inflight_turns(&http, &shared, &provider).await;

    let state = inflight::load_inflight_state(&provider, original.channel_id)
        .expect("failed relay keeps debt");
    assert!(state.replay_rerun_blocked());
    assert_eq!(state.full_response, original.full_response);
    assert_eq!(state.user_text, original.user_text);
    assert_eq!(state.session_id, original.session_id);
    assert_eq!(
        state.streaming_rollover_frozen_msg_ids,
        original.streaming_rollover_frozen_msg_ids
    );
    assert_eq!(state.response_sent_offset, original.response_sent_offset);
    assert!(!state.terminal_delivery_completed());
    assert!(state.recovery_relay_attempts > original.recovery_relay_attempts);
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    server.abort();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_dormant_rows_keep_existing_missing_host_disposal_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let channel = 1_479_671_301_387_173_000;
    let name = provider.build_tmux_session_name("replay-dormant");
    seed(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel,
        Stored::Legacy,
    )
    .await;
    let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent);
    let _presence = InjectedPresenceGuard::set(HostSessionRef::tmux(&name), HostPresence::Missing);
    let mut original = row(channel, name);
    original.replay_receipt_id = Some(receipt(&pool, &original, "registered_not_started").await);
    inflight::save_inflight_state_create_new(&original).unwrap();
    let discord = o_cut_recorder::start(channel).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    assert!(inflight::load_inflight_state(&provider, channel).is_none());
    assert!(
        discord
            .contents()
            .iter()
            .any(|body| body.contains("saved partial result"))
    );
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_read_error_is_not_a_durable_hold_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let provider = ProviderKind::Claude;
    let channel = 1_479_671_301_387_178_000;
    let name = provider.build_tmux_session_name("replay-read-error");
    let shared = shared_on(&pool).await;
    seed(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel,
        Stored::Legacy,
    )
    .await;
    let original = row(channel, name.clone());
    inflight::save_inflight_state_create_new(&original).unwrap();
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &provider,
        channel,
    );
    let before = std::fs::read(&path).unwrap();
    let closed = sqlx::PgPool::connect(&db.database_url).await.unwrap();
    closed.close().await;
    let unavailable = shared_on(&closed).await;
    let discord = o_cut_recorder::start(channel).await;
    {
        let _pane =
            InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::ProbeError);
        let _presence =
            InjectedPresenceGuard::set(HostSessionRef::tmux(&name), HostPresence::ProbeFailed);
        restore_inflight_turns(&discord.http, &unavailable, &provider).await;
    }
    let kept = inflight::load_inflight_state(&provider, channel)
        .expect("the existing failed host probe keeps the row");
    assert!(
        kept.replay_hold_reasons.is_empty(),
        "read errors are not durable holds"
    );
    assert_eq!(kept.replay_receipt_id, None);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(discord.contents().is_empty());
    assert!(unavailable.core.lock().await.sessions.is_empty());
    assert_eq!(unavailable.tmux_watchers.len(), 0);

    let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent);
    let _presence = InjectedPresenceGuard::set(HostSessionRef::tmux(&name), HostPresence::Missing);
    restore_inflight_turns(&discord.http, &shared, &provider).await;
    assert!(inflight::load_inflight_state(&provider, channel).is_none());
    assert!(
        discord
            .contents()
            .iter()
            .any(|body| body.contains("saved partial result")),
        "a healthy second pass must take the existing missing-host disposal"
    );
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_projected_read_errors_keep_bytes_until_authority_recovers_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let provider = ProviderKind::Claude;
    let shared = shared_on(&pool).await;
    let closed = sqlx::PgPool::connect(&db.database_url).await.unwrap();
    closed.close().await;
    let unavailable = [
        shared_on(&closed).await,
        crate::services::discord::make_shared_data_for_tests_with_storage(None),
    ];
    let mut rows = Vec::new();
    let mut probes = Vec::new();
    for n in 0..2 {
        let channel = 1_479_671_301_387_179_000 + n;
        let name = provider.build_tmux_session_name(&format!("replay-projected-read-error-{n}"));
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
        probes.push((
            InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent),
            InjectedPresenceGuard::set(HostSessionRef::tmux(&name), HostPresence::Missing),
        ));
        let mut original = row(channel, name);
        original.replay_receipt_id =
            Some(receipt(&pool, &original, "registered_not_started").await);
        inflight::save_inflight_state_create_new(&original).unwrap();
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &provider,
            channel,
        );
        let bytes = std::fs::read(&path).unwrap();
        rows.push((original, path, bytes));
    }
    let discord = o_cut_recorder::start(rows[0].0.channel_id).await;
    for unavailable in unavailable {
        unavailable.settings.write().await.provider = provider.clone();
        restore_inflight_turns(&discord.http, &unavailable, &provider).await;
        for (original, path, bytes) in &rows {
            let kept = inflight::load_inflight_state(&provider, original.channel_id).unwrap();
            assert_eq!(kept.replay_receipt_id, original.replay_receipt_id);
            assert!(kept.replay_hold_reasons.is_empty());
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
        }
        assert!(discord.contents().is_empty());
        assert!(unavailable.core.lock().await.sessions.is_empty());
        assert_eq!(unavailable.tmux_watchers.len(), 0);
    }
    restore_inflight_turns(&discord.http, &shared, &provider).await;
    for (original, ..) in rows {
        assert!(inflight::load_inflight_state(&provider, original.channel_id).is_none());
    }
    assert_eq!(
        discord
            .contents()
            .iter()
            .filter(|body| body.contains("saved partial result"))
            .count(),
        2,
        "read recovery must resume the existing dormant disposal"
    );
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    drop(probes);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_preserves_unknown_runtime_bytes_before_legacy_clear_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let mut raw_rows = Vec::new();
    for n in 0..2 {
        let mut original = row(
            1_479_671_301_387_174_000 + n,
            format!("future-runtime-held-{n}"),
        );
        original.replay_receipt_id = Some(receipt(&pool, &original, "withheld").await);
        inflight::save_inflight_state_create_new(&original).unwrap();
        let root = inflight::inflight_runtime_root().unwrap();
        let path = inflight::inflight_state_path(&root, &provider, original.channel_id);
        let mut forward: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if n == 0 {
            forward["runtime_kind"] = serde_json::json!("future_runtime_v2");
        } else {
            forward["runtime_kind"] = serde_json::json!("legacy_tmux_wrapper");
            forward["version"] = serde_json::json!(inflight::inflight_state_version() + 1);
            forward["future_delivery_proof"] = serde_json::json!({"unrecognized": "must retain"});
        }
        let bytes = serde_json::to_vec(&forward).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        raw_rows.push((path, bytes));
    }
    let discord = o_cut_recorder::start(1_479_671_301_387_174_000).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    for (path, bytes) in raw_rows {
        assert_eq!(
            std::fs::read(&path).expect("held forward row must not clear or erase future fields"),
            bytes
        );
    }
    assert!(discord.contents().is_empty());
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_preserves_a_fresh_actor_while_old_debt_waits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(1_479_671_301_387_175_000);
    let mut original = row(channel.get(), "fresh-actor-old-held-debt".into());
    original.user_msg_id += 100;
    original.finalizer_turn_id = original.user_msg_id;
    original.replay_receipt_id = Some(receipt(&pool, &original, "withheld").await);
    inflight::save_inflight_state_create_new(&original).unwrap();
    let actor = nameless_turn(&shared, channel).await;
    let discord = o_cut_recorder::start(channel.get()).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    let state = inflight::load_inflight_state(&provider, channel.get())
        .expect("older held debt must remain");
    assert_eq!(state.full_response, original.full_response);
    assert_eq!(state.response_sent_offset, original.response_sent_offset);
    assert!(!state.terminal_delivery_completed());
    assert!(discord.contents().is_empty());
    assert!(!actor.cancelled.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .active_user_message_id,
        Some(MessageId::new(channel.get() + 1))
    );
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_keeps_already_visible_prefix_at_the_same_anchor_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let mut original = row(1_479_671_301_387_176_000, "held-current-anchor".into());
    original.streaming_rollover_frozen_msg_ids.clear();
    original.full_response = "current anchor prefix\nsaved partial result".into();
    original.response_sent_offset = "current anchor prefix\n".len();
    original.replay_receipt_id = Some(receipt(&pool, &original, "withheld").await);
    inflight::save_inflight_state_create_new(&original).unwrap();
    let discord = o_cut_recorder::start(original.channel_id).await;
    crate::services::discord::http::edit_channel_message(
        &discord.http,
        ChannelId::new(original.channel_id),
        MessageId::new(original.current_msg_id),
        "current anchor prefix\n",
    )
    .await
    .unwrap();

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    let bodies = discord.contents();
    let replaced = bodies
        .last()
        .expect("the held debt should replace its captured anchor");
    assert!(
        replaced.contains("current anchor prefix"),
        "replacing the current anchor must retain its visible prefix"
    );
    assert!(replaced.contains("saved partial result"));
    let state = inflight::load_inflight_state(&provider, original.channel_id)
        .expect("held episode must remain");
    assert_eq!(state.full_response, original.full_response);
    assert_eq!(state.current_msg_id, original.current_msg_id);
    assert!(state.terminal_delivery_completed());
    assert!(state.streaming_rollover_frozen_msg_ids.is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn replay_hold_restore_retains_ambiguous_or_invalid_frozen_debt_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let mut originals = Vec::new();
    for n in 0..2 {
        let channel = 1_479_671_301_387_177_000 + n;
        let mut original = row(channel, format!("ambiguous-held-{n}"));
        original.full_response = "한글 prefix\nsaved partial result".into();
        original.response_sent_offset = 0;
        original.replay_receipt_id = Some(receipt(&pool, &original, "withheld").await);
        inflight::save_inflight_state_create_new(&original).unwrap();
        if n == 1 {
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &provider,
                channel,
            );
            let mut raw: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            raw["response_sent_offset"] = serde_json::json!(1);
            std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
            original.response_sent_offset = 1;
        }
        originals.push(original);
    }
    let discord = o_cut_recorder::start(originals[0].channel_id).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    for original in &originals {
        let state = inflight::load_inflight_state(&provider, original.channel_id)
            .expect("unclassified delivery boundary must retain debt");
        assert_eq!(state.full_response, original.full_response);
        assert_eq!(state.user_text, original.user_text);
        assert_eq!(state.response_sent_offset, original.response_sent_offset);
        assert_eq!(
            state.streaming_rollover_frozen_msg_ids,
            original.streaming_rollover_frozen_msg_ids
        );
        assert!(!state.terminal_delivery_completed());
    }
    assert!(discord.contents().is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p3_restart_backfills_old_withheld_without_readopting_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let channel = 1_479_671_301_387_171_100;
    let _active = crate::services::discord::router::replay_test_activation::activate(channel);
    let mut state = row(channel, "p3-old-held".into());
    state.full_response.clear();
    state.response_sent_offset = 0;
    let id = receipt(&pool, &state, "withheld").await;
    state.replay_receipt_id = Some(id);
    inflight::save_inflight_state_create_new(&state).unwrap();
    let discord = o_cut_recorder::start(channel).await;
    for _ in 0..2 {
        restore_inflight_turns(&discord.http, &shared, &ProviderKind::Claude).await;
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox WHERE reason_code='stale_replay_withheld'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 1,
        "restart must supplement exactly one durable notice"
    );
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "recovery cannot adopt a provider"
    );
    let preserved: serde_json::Value =
        sqlx::query_scalar("SELECT replay_preserved FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        preserved["recovery_snapshot"]["channel_id"], state.channel_id,
        "local recovery must capture its episode before the PG-only sweep"
    );
    assert_eq!(preserved["recovery_snapshot"]["user_text"], state.user_text);

    assert!(
        !shared.tmux_watchers.contains_key(&ChannelId::new(channel)),
        "recovery cannot restart the wrapper"
    );
    let stored: (String, Option<String>, String) = sqlx::query_as("SELECT replay_disposition,replay_owner_incarnation,user_text FROM intake_outbox WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(stored, ("withheld".into(), None, state.user_text));
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_pg_only_restart_paginates_without_an_execution_permit_pg() {
    const CHILD: &str = "ADK_REPLAY_RECOVERY_CHILD";
    let _root = crate::config::TestRuntimeRootGuard::new();
    if let Ok(raw) = std::env::var(CHILD) {
        let (url, channels): (String, Vec<u64>) = serde_json::from_str(&raw).unwrap();
        let _active: Vec<_> = channels
            .iter()
            .map(|&id| crate::services::discord::router::replay_test_activation::activate(id))
            .collect();
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let probe = tempfile::tempdir().unwrap();
        let cli = probe.path().join("native-cli");
        std::fs::write(&cli, "#!/usr/bin/python3\nimport os,sys\nfrom pathlib import Path\nif '--version' in sys.argv or '-v' in sys.argv: print('99.1.0');sys.exit(0)\nwith (Path(os.environ['ADK_RECOVERY_PROBE_CWD'])/'native_starts').open('a') as f: f.write(str(os.getpid())+'\\n')\nsys.stdin.read()\nprint('{\"type\":\"turn.completed\",\"usage\":{}}')\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _cli = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CODEX_PATH",
            &cli,
        );
        let _cwd = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "ADK_RECOVERY_PROBE_CWD",
            probe.path(),
        );
        execution_probe();
        let starts = || {
            std::fs::read_to_string(probe.path().join("native_starts"))
                .unwrap()
                .lines()
                .count()
        };
        assert_eq!(
            starts(),
            1,
            "positive control must cross the native provider entry"
        );
        let shared = shared_on(&pool).await;
        let discord = o_cut_recorder::start(channels[0]).await;
        for _ in 0..2 {
            restore_inflight_turns(&discord.http, &shared, &ProviderKind::Claude).await;
        }
        let counts: (i64, i64) = sqlx::query_as("SELECT COUNT(*), COUNT(DISTINCT replay_preserved -> 'stale_replay_notice' ->> 'outbox_id') FROM intake_outbox WHERE replay_disposition='withheld'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (channels.len() as i64, channels.len() as i64));
        assert!(shared.core.lock().await.sessions.is_empty());
        assert!(
            channels
                .iter()
                .all(|&id| !shared.tmux_watchers.contains_key(&ChannelId::new(id)))
        );
        assert!(
            discord.calls().is_empty(),
            "one durable event, no direct recovery POST"
        );
        assert!(inflight::load_inflight_states(&ProviderKind::Claude).is_empty());
        assert_eq!(
            starts(),
            1,
            "restart supplementation must cause no additional native execution"
        );
        println!("RECOVERY_FRESH_NODE_PG_ONLY_OK {}", channels.len());
        pool.close().await;
        return;
    }
    let (db, pool) = fixture().await;
    let mut channels = Vec::new();
    for n in 0..67_u64 {
        let channel = 1_479_671_301_387_180_000 + n * 10;
        let mut state = row(channel, "pg-only-no-runtime".into());
        state.turn_nonce = (n != 0).then(|| format!("old-nonce-{n}"));
        receipt(
            &pool,
            &state,
            match n {
                1 => "started_unclassified",
                2 => "startup_failed_no_effect",
                _ => "withheld",
            },
        )
        .await;
        channels.push(channel);
    }
    for _ in 0..2 {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                concat!(
                    module_path!(),
                    "::recovery_pg_only_restart_paginates_without_an_execution_permit_pg"
                )
                .split_once("::")
                .unwrap()
                .1,
                "--nocapture",
            ])
            .env(
                CHILD,
                serde_json::to_string(&(&db.database_url, &channels)).unwrap(),
            )
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "fresh-node child: {} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("RECOVERY_FRESH_NODE_PG_ONLY_OK 67")
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 67,
        "restarts and pages must neither merge events nor duplicate them"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_malformed_notice_retains_raw_episode_and_has_a_healthy_control_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let mut guards = Vec::new();
    let mut damaged = None;
    for (n, malformed) in [true, false].into_iter().enumerate() {
        let channel = 1_479_671_301_387_181_000 + n as u64 * 10;
        guards.push(crate::services::discord::router::replay_test_activation::activate(channel));
        let mut state = row(channel, "malformed-no-runtime".into());
        state.full_response.clear();
        state.response_sent_offset = 0;
        let id = receipt(
            &pool,
            &state,
            if malformed {
                "started_unclassified"
            } else {
                "withheld"
            },
        )
        .await;
        if malformed {
            sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld',replay_preserved='[]'::jsonb WHERE id=$1").bind(id).execute(&pool).await.unwrap();
        }
        state.replay_receipt_id = Some(id);
        inflight::save_inflight_state_create_new(&state).unwrap();
        if malformed {
            damaged = Some((
                state.clone(),
                std::fs::read(inflight::inflight_state_path(
                    &inflight::inflight_runtime_root().unwrap(),
                    &ProviderKind::Claude,
                    channel,
                ))
                .unwrap(),
            ));
        }
    }
    let discord = o_cut_recorder::start(1_479_671_301_387_181_000).await;
    restore_inflight_turns(&discord.http, &shared, &ProviderKind::Claude).await;
    let (state, bytes) = damaged.unwrap();
    assert_eq!(
        std::fs::read(inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            state.channel_id
        ))
        .unwrap(),
        bytes
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "healthy control must publish; malformed row cannot become no debt"
    );
    assert!(matches!(
        crate::db::replay_disposition::hold_notice::read(&pool, state.replay_receipt_id.unwrap())
            .await,
        Err(crate::db::replay_disposition::hold_notice::NoticeReadError::Malformed)
    ));
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_history_retry_reads_the_canonical_fence_before_and_after_http_pg() {
    use crate::services::discord::turn_bridge::recovery_text::{
        auto_retry_with_history, release_retry_pending,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let hits = Arc::new(AtomicUsize::new(0));
    let (on_reach, on_release, on_hit) = (reached.clone(), release.clone(), hits.clone());
    let app = axum::Router::new().fallback(move || {
        let (r, g, h) = (on_reach.clone(), on_release.clone(), on_hit.clone());
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            r.notify_one();
            g.notified().await;
            axum::Json(serde_json::json!([]))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (n, when) in ["before", "during", "unprotected"].into_iter().enumerate() {
        let channel = 1_479_671_301_387_182_000 + n as u64 * 10;
        let _active = crate::services::discord::router::replay_test_activation::activate(channel);
        let state = row(channel, "canonical-history-no-runtime".into());
        if when == "before" {
            receipt(&pool, &state, "withheld").await;
        }
        let task = {
            let (http, shared) = (http.clone(), shared.clone());
            let text = state.user_text.clone();
            tokio::spawn(async move {
                auto_retry_with_history(
                    &http,
                    &shared,
                    &ProviderKind::Claude,
                    ChannelId::new(channel),
                    MessageId::new(channel + 1),
                    &text,
                )
                .await
            })
        };
        if when != "before" {
            tokio::time::timeout(std::time::Duration::from_secs(10), reached.notified())
                .await
                .unwrap();
            if when == "during" {
                receipt(&pool, &state, "withheld").await;
            }
            release.notify_one();
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let mailbox = shared.mailbox(ChannelId::new(channel)).snapshot().await;
        assert_eq!(
            mailbox
                .intervention_queue
                .iter()
                .any(|entry| entry.message_id.get() == state.user_msg_id),
            when == "unprotected",
            "{when}: canonical fence must win even with an empty memory map"
        );
        release_retry_pending(ChannelId::new(channel));
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "blocked-before case does not fetch HTTP; healthy control actually enqueues"
    );
    server.abort();
    pool.close().await;
    db.drop().await;
}

// The positive control and the execution mutant cross the same public native provider entry.
pub(super) fn execution_probe() {
    let cwd = std::env::var("ADK_RECOVERY_PROBE_CWD").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    crate::services::codex::execute_command_streaming(
        "probe",
        Some("old-native"),
        &cwd,
        tx,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();
    assert!(
        rx.try_iter().any(|message| matches!(
            message,
            crate::services::agent_protocol::StreamMessage::Done { .. }
        )),
        "actual native control must complete"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_unprojected_lookup_failure_retains_bytes_until_authority_returns_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = fixture().await;
    let channel = 1_479_671_301_387_184_000;
    let _active = crate::services::discord::router::replay_test_activation::activate(channel);
    let mut state = row(channel, "read-error-no-runtime".into());
    state.full_response.clear();
    state.response_sent_offset = 0;
    let id = receipt(&pool, &state, "withheld").await;
    assert_eq!(state.replay_receipt_id, None);
    inflight::save_inflight_state_create_new(&state).unwrap();
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        channel,
    );
    let before = std::fs::read(&path).unwrap();
    let unavailable = sqlx::PgPool::connect(&db.database_url).await.unwrap();
    unavailable.close().await;
    let failed = shared_on(&unavailable).await;
    let discord = o_cut_recorder::start(channel).await;
    restore_inflight_turns(&discord.http, &failed, &ProviderKind::Claude).await;
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "unverified cannot mean no protected request"
    );
    assert!(failed.core.lock().await.sessions.is_empty());
    let healthy = shared_on(&pool).await;
    restore_inflight_turns(&discord.http, &healthy, &ProviderKind::Claude).await;
    assert_eq!(
        inflight::load_inflight_state(&ProviderKind::Claude, channel)
            .unwrap()
            .replay_receipt_id,
        Some(id)
    );
    let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        notices, 1,
        "a transient failure must recover to one durable event"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p4_dormant_no_store_history_retry_still_fetches_and_enqueues() {
    use crate::services::discord::turn_bridge::recovery_text::{
        auto_retry_with_history, release_retry_pending,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let shared = crate::services::discord::make_shared_data_for_tests();
    assert!(shared.pg_pool.is_none());
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = hits.clone();
    let app = axum::Router::new().fallback(move || {
        let observed = observed.clone();
        async move {
            observed.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!([]))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = serenity::HttpBuilder::new("test-token")
        .proxy(format!("http://{}", listener.local_addr().unwrap()))
        .ratelimiter_disabled(true)
        .build();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let channel = ChannelId::new(7_330_431);
    let user = MessageId::new(7_330_432);
    auto_retry_with_history(
        &http,
        &shared,
        &ProviderKind::Claude,
        channel,
        user,
        "original",
    )
    .await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "off must reach the actual history GET without PG"
    );
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert!(
        snapshot
            .intervention_queue
            .iter()
            .any(|item| item.message_id == user)
    );
    release_retry_pending(channel);
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn p5_off_boot_pg_only_resumes_without_an_execution_permit_pg() {
    const CHILD: &str = "ADK_REPLAY_RECOVERY_CHILD";
    let _root = crate::config::TestRuntimeRootGuard::new();
    if let Ok(raw) = std::env::var(CHILD) {
        let (url, channels): (String, Vec<u64>) = serde_json::from_str(&raw).unwrap();
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let probe = tempfile::tempdir().unwrap();
        let cli = probe.path().join("native-cli");
        std::fs::write(&cli, "#!/usr/bin/python3\nimport os,sys\nfrom pathlib import Path\nif '--version' in sys.argv or '-v' in sys.argv: print('99.1.0');sys.exit(0)\nwith (Path(os.environ['ADK_RECOVERY_PROBE_CWD'])/'native_starts').open('a') as f: f.write(str(os.getpid())+'\\n')\nsys.stdin.read()\nprint('{\"type\":\"turn.completed\",\"usage\":{}}')\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _cli = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CODEX_PATH",
            &cli,
        );
        let _cwd = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "ADK_RECOVERY_PROBE_CWD",
            probe.path(),
        );
        execution_probe();
        let starts = || {
            std::fs::read_to_string(probe.path().join("native_starts"))
                .unwrap()
                .lines()
                .count()
        };
        assert_eq!(
            starts(),
            1,
            "positive control must cross the native provider entry"
        );
        let shared = shared_on(&pool).await;
        let discord = o_cut_recorder::start(channels[0]).await;
        for _ in 0..2 {
            restore_inflight_turns(&discord.http, &shared, &ProviderKind::Claude).await;
        }
        let counts: (i64, i64) = sqlx::query_as("SELECT COUNT(*), COUNT(DISTINCT replay_preserved -> 'stale_replay_notice' ->> 'outbox_id') FROM intake_outbox WHERE replay_disposition='withheld'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (channels.len() as i64, channels.len() as i64));
        assert!(shared.core.lock().await.sessions.is_empty());
        assert!(
            channels
                .iter()
                .all(|&id| !shared.tmux_watchers.contains_key(&ChannelId::new(id)))
        );
        assert!(
            discord.calls().is_empty(),
            "one durable event, no direct recovery POST"
        );
        assert!(inflight::load_inflight_states(&ProviderKind::Claude).is_empty());
        assert_eq!(
            starts(),
            1,
            "restart supplementation must cause no additional native execution"
        );
        println!("RECOVERY_FRESH_NODE_PG_ONLY_OK {}", channels.len());
        pool.close().await;
        return;
    }
    let (db, pool) = fixture().await;
    let mut channels = Vec::new();
    for n in 0..67_u64 {
        let channel = 1_479_671_301_387_180_000 + n * 10;
        let mut state = row(channel, "pg-only-no-runtime".into());
        state.turn_nonce = (n != 0).then(|| format!("old-nonce-{n}"));
        receipt(
            &pool,
            &state,
            match n {
                1 => "started_unclassified",
                2 => "startup_failed_no_effect",
                _ => "withheld",
            },
        )
        .await;
        channels.push(channel);
    }
    for _ in 0..2 {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .env("AGENTDESK_STALE_REPLAY_V1", "0")
            .args([
                "--exact",
                concat!(
                    module_path!(),
                    "::p5_off_boot_pg_only_resumes_without_an_execution_permit_pg"
                )
                .split_once("::")
                .unwrap()
                .1,
                "--nocapture",
            ])
            .env(
                CHILD,
                serde_json::to_string(&(&db.database_url, &channels)).unwrap(),
            )
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "fresh-node child: {} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("RECOVERY_FRESH_NODE_PG_ONLY_OK 67")
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 67,
        "restarts and pages must neither merge events nor duplicate them"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p5_empty_boot_scan_has_one_read_and_no_effect_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
    let discord = o_cut_recorder::start(7_330_505).await;
    let key = format!(
        "scan:claude:{}",
        pool.connect_options().get_database().unwrap()
    );
    let before = crate::services::discord::replay_policy::activation::reads(&key);
    restore_inflight_turns(&discord.http, &shared, &ProviderKind::Claude).await;
    assert_eq!(
        crate::services::discord::replay_policy::activation::reads(&key) - before,
        1
    );
    assert!(discord.calls().is_empty());
    assert!(shared.core.lock().await.sessions.is_empty());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p5_off_legacy_lookup_error_retries_until_authority_returns_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = fixture().await;
    let state = row(7_330_517, "no-running-host".into());
    receipt(&pool, &state, "withheld").await;
    let shared = shared_on(&pool).await;
    let discord = o_cut_recorder::start(state.channel_id).await;
    sqlx::query("ALTER TABLE intake_outbox RENAME TO p5_retry_unavailable")
        .execute(&pool)
        .await
        .unwrap();
    let waiting = crate::services::discord::turn_bridge::recovery_text::auto_retry_with_history(
        &discord.http,
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(state.channel_id),
        poise::serenity_prelude::MessageId::new(state.user_msg_id),
        "original",
    );
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut waiting)
            .await
            .is_err(),
        "query errors remain a scheduled retry, never an empty result"
    );
    assert!(discord.calls().is_empty());
    sqlx::query("ALTER TABLE p5_retry_unavailable RENAME TO intake_outbox")
        .execute(&pool)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), waiting)
        .await
        .unwrap();
    assert!(
        discord.calls().is_empty(),
        "a restored held receipt stops before history fetch"
    );
    pool.close().await;
    db.drop().await;
}
