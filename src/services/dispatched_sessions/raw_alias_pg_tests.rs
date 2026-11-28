//! A key an operator still holds as an alias of a promoted row acts on the row's live session.

use crate::services::discord::session_locator::tests::PromotionRig;
use crate::services::provider::ProviderKind;
use crate::services::provider::session_source::{session_provider, tmux_session_provider};
use axum::http::StatusCode;

#[tokio::test]
async fn raw_alias_kill_tmux_ends_current_session_pg() {
    let rig = PromotionRig::new(81190321).await;
    let (state, headers) = (rig.app_state(), axum::http::HeaderMap::new());
    let (status, body) = crate::services::dispatched_sessions::kill_tmux_session_impl(
        &state,
        &headers,
        &rig.old,
        "manual cleanup",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.0["tmux_killed"], true, "{body:?}");
    assert!(rig.others().is_empty(), "{:?}", rig.others());
    rig.finish().await;
}

#[tokio::test]
async fn raw_alias_reconcile_follows_current_primary_pg() {
    let rig = PromotionRig::new(81190322).await;
    let name = format!("={}", rig.name);
    assert!(rig.raw(&["kill-session", "-t", &name]).status.success());
    sqlx::query("UPDATE sessions SET status='turn_active', last_heartbeat=NOW()-INTERVAL '2 hours' WHERE id=$1")
        .bind(rig.id)
        .execute(&rig.pool)
        .await
        .unwrap();
    let reply = crate::server::routes::dispatched_sessions::reconcile_stale_turn(
        axum::extract::State(rig.app_state()),
        axum::extract::Path(rig.old.clone()),
    )
    .await;
    let (status, body) = reply.unwrap_or_else(|_| panic!("the alias must reach its row"));
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.0["reconciled"], true, "{body:?}");
    assert_eq!(rig.status().await, "idle");
    rig.finish().await;
}

#[tokio::test]
async fn raw_alias_resume_tears_down_current_session_pg() {
    let rig = PromotionRig::new(1_481_190_323_000_000_000).await;
    sqlx::query("UPDATE sessions SET thread_channel_id=channel_id, cwd=$2, claude_session_id='old' WHERE id=$1")
        .bind(rig.id)
        .bind(rig.root.path().to_str().unwrap())
        .execute(&rig.pool)
        .await
        .unwrap();
    let target = "55555555-5555-5555-5555-555555555555";
    let options = crate::services::session_resume::ResumePreviousOptions {
        session_id: Some(target.to_string()),
        cwd: Some(rig.root.path().to_str().unwrap().to_string()),
    };
    let home = rig.root.path().join("claude-home");
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        rig.root.path(),
        target,
        Some(&home),
    )
    .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(transcript, b"{}\n").unwrap();
    let state = rig.app_state();
    let context = crate::services::session_forwarding::ForwardCallerContext::from(&state);
    let resume = crate::services::session_resume::dispatch_resume_previous(
        &rig.pool,
        crate::services::discord::health::resume_ports(Some(&rig.registry)),
        &context,
        None,
        &rig.old,
        &options,
    );
    let (status, body) =
        crate::services::session_resume::catalog::scope_claude_home_for_tests(home, resume).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(
        rig.others().is_empty(),
        "the rebind must end the session it replaces: {body:?}"
    );
    let bound: Option<String> =
        sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE id=$1")
            .bind(rig.id)
            .fetch_one(&rig.pool)
            .await
            .unwrap();
    assert_eq!(bound.as_deref(), Some(target));
    rig.finish().await;
}

#[tokio::test]
async fn new_name_provider_is_read_from_records_never_guessed_pg() {
    let mut rig = PromotionRig::new(81190324).await;
    let (codex, bare) = (
        format!("s3-codex-{}", rig.id),
        format!("s3-bare-{}", rig.id),
    );
    let channel = Some(rig.channel.get());
    let marker = crate::services::session_identity_metadata::provider_identity(
        &codex, "codex", channel, "channel",
    )
    .and_then(|identity| identity.environment_assignment())
    .unwrap();
    rig.start(&codex, Some(&marker));
    rig.start(&bare, None);
    assert_eq!(tmux_session_provider(&codex), Some(ProviderKind::Codex));
    assert_eq!(tmux_session_provider(&rig.name), Some(ProviderKind::Claude));
    assert_eq!(
        tmux_session_provider(&bare),
        None,
        "an unmarked name records none"
    );
    let keyed = format!("codex/hash/host:{bare}");
    assert_eq!(
        session_provider(None, Some(&keyed), &bare),
        Some(ProviderKind::Codex)
    );
    assert_eq!(
        session_provider(Some("codex"), Some(&rig.key), &rig.name),
        Some(ProviderKind::Codex)
    );
    rig.finish().await;
}
