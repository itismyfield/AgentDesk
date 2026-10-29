use super::*;
use crate::services::cluster::intake_router_hook::owner_record::snapshot::tests::Fixture;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};

fn binding(provider: &str, channel: u64, owner: &str) -> (PreparedIncarnation, Value) {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let prepared = PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: provider.into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.clone(),
        tmux_session: owner.into(),
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
    let marker = crate::services::tmux_common::session_temp_path(owner, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &nonce).unwrap();
    (
        prepared,
        json!({"host":"tmux","endpoint":"mac-a","owner":owner,"root":root,"execution_nonce":nonce}),
    )
}

#[tokio::test]
async fn original_adoption_binding_history_and_legacy_artifacts_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new("73369201").await;
    for (index, provider) in ["claude", "codex"].into_iter().enumerate() {
        let channel = 73369210 + index as u64;
        let id = OwnerIdentity::new(provider, &channel.to_string());
        let owner = format!("pd1b-adoption-{provider}");
        let locator = format!("{provider}/token/mac-a:{owner}");
        let (_prepared, native) = binding(provider, channel, &owner);
        sqlx::query("INSERT INTO sessions(session_key,provider,channel_id,instance_id,status) VALUES($1,$2,$3,'mac-a','idle')")
            .bind(&locator).bind(provider).bind(id.raw_channel_id()).execute(&f.pool).await.unwrap();
        let artifact: i64 = sqlx::query_scalar("INSERT INTO message_outbox(target,content,bot,source) VALUES($1,'old artifact','notify','headless_turn') RETURNING id")
            .bind(format!("channel:{channel}")).fetch_one(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(
            adopt_on_tx(&mut tx, &id, "mac-a", &locator, &native)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(
            adopt_on_tx(&mut tx, &id, "mac-a", &locator, &native)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        let (generation, adopted, original): (i64, bool, Value) = sqlx::query_as("SELECT generation,adopted_from_session,routing_adoption FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2")
            .bind(provider).bind(id.raw_channel_id()).fetch_one(&f.pool).await.unwrap();
        assert_eq!((generation, adopted), (0, true));
        assert_eq!(original["binding"], native);
        assert_eq!(original["session_key"], locator);
        let old_origin: Option<Value> =
            sqlx::query_scalar("SELECT output_origin FROM message_outbox WHERE id=$1")
                .bind(artifact)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert!(
            old_origin.is_none(),
            "adoption must never restamp a legacy artifact"
        );
        sqlx::query("UPDATE intake_session_owners SET status='released' WHERE provider=$1 AND raw_channel_id=$2")
            .bind(provider).bind(id.raw_channel_id()).execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        assert!(
            !adopt_on_tx(&mut tx, &id, "mac-a", &locator, &native)
                .await
                .unwrap()
        );
        tx.rollback().await.unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2",
        )
        .bind(provider)
        .bind(id.raw_channel_id())
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }
    f.finish().await;
}

#[tokio::test]
async fn adoption_refuses_unverified_native_and_conflicting_session_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new("73369202").await;
    let id = OwnerIdentity::new("claude", &f.channel);
    let owner = "pd1b-adoption-refusal";
    let locator = format!("claude/token/mac-a:{owner}");
    let (_prepared, native) = binding("claude", f.channel.parse().unwrap(), owner);
    sqlx::query("INSERT INTO sessions(session_key,provider,channel_id,instance_id,status) VALUES($1,'claude',$2,'mac-a','idle')")
        .bind(&locator).bind(&f.channel).execute(&f.pool).await.unwrap();
    let mut wrong = native.clone();
    wrong["execution_nonce"] = json!("other-incarnation");
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        adopt_on_tx(&mut tx, &id, "mac-a", &locator, &wrong)
            .await
            .is_err(),
        "unverified native identity must not become an owner"
    );
    tx.rollback().await.unwrap();
    sqlx::query("INSERT INTO sessions(session_key,provider,channel_id,instance_id,status) VALUES('foreign-locator','claude',$1,'mac-b','idle')")
        .bind(&f.channel).execute(&f.pool).await.unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        !adopt_on_tx(&mut tx, &id, "mac-a", &locator, &native)
            .await
            .unwrap()
    );
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_session_owners")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    f.finish().await;
}
