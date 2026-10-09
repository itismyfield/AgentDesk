//! Actual restart scan: a durable hold preserves the episode while delivering its saved debt.

use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, nameless_turn, seed, shared_on,
};
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
use poise::serenity_prelude::ChannelId;

async fn fixture() -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
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
    let discord = o_cut_recorder::start(original.channel_id).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    let state = inflight::load_inflight_state(&provider, original.channel_id)
        .expect("missing authority must keep debt");
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
