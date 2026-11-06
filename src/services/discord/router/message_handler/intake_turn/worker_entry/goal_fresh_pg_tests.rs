//! `/goal --fresh` through the real intake entry on a live Codex channel: when its durable context
//! boundary does not land, the turn stops before it clears the provider session or launches.

use super::*;
use crate::services::codex::CodexSourceMode;
use crate::services::codex_tui::canary::CANARY_CHANNEL;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::{
    CanaryFixture, NATIVE_ID,
};

#[tokio::test(flavor = "current_thread")]
async fn intake_goal_fresh_keeps_the_session_when_its_boundary_does_not_land_pg() {
    let name = concat!(
        module_path!(),
        "::intake_goal_fresh_keeps_the_session_when_its_boundary_does_not_land_pg"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let mut fixture = CanaryFixture::new_in(CodexSourceMode::Verified).await;
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        |method, path| {
            (method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{CANARY_CHANNEL}")))
            .then(|| {
                serde_json::json!({
                    "id": CANARY_CHANNEL.to_string(), "type": 0,
                    "name": "adk-codex-tui-e2e", "guild_id": "42000",
                    "position": 0, "permission_overwrites": [], "nsfw": false,
                    "parent_id": null
                })
            })
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
    let pool = fixture.shared.pg_pool.clone().unwrap();
    for statement in [
        "CREATE FUNCTION reject_fresh_boundary() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'injected fresh boundary failure'; END; $$ LANGUAGE plpgsql",
        "CREATE TRIGGER reject_fresh_boundary BEFORE INSERT OR UPDATE
         ON channel_session_clear_boundaries
         FOR EACH ROW EXECUTE FUNCTION reject_fresh_boundary()",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    let channel = ChannelId::new(CANARY_CHANNEL);
    let message = MessageId::new(CANARY_CHANNEL + 61);
    let request = IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: message,
        source_message_ids: Vec::new(),
        busy_followup_retry_user_msg_id: message,
        request_owner: UserId::new(7),
        request_owner_name: "fixture owner".into(),
        user_text: "/goal --fresh rebuild the plan from scratch".into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: false,
    };
    let entry = execute_intake_turn_core(&api.http, &fixture.shared, "", request, Vec::new());
    let error = tokio::time::timeout(std::time::Duration::from_secs(30), entry)
        .await
        .expect("intake returns")
        .expect_err("a fresh goal whose boundary did not land must not proceed");
    assert!(
        format!("{error:#}").contains("injected fresh boundary failure"),
        "{error:#}"
    );
    let session = fixture.shared.core.lock().await.sessions[&channel]
        .session_id
        .clone();
    assert_eq!(
        session.as_deref(),
        Some(NATIVE_ID),
        "the provider session is not cleared before its boundary lands"
    );
    let launches = (fixture.fake.take_calls().iter())
        .filter(|call| call.starts_with("new-session "))
        .count();
    assert_eq!(launches, 0, "nothing launches without the boundary");
    let boundaries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM channel_session_clear_boundaries WHERE channel_id = $1",
    )
    .bind(channel.get().to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(boundaries, 0);
}
