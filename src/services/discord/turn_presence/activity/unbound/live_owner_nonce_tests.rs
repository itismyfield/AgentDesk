//! File and receipt changes must invalidate a saved real Reading without another Activity read.
use super::*;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::settled_owner_observation;

#[tokio::test(flavor = "current_thread")]
async fn a2_foreign_spawn_nonce_only_change_rejects_saved_idle_publication_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_foreign_spawn_nonce_only_change_rejects_saved_idle_publication_pg"
    )) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_121, ProviderKind::Claude).await;
    fixture.produce_owners();
    let marker = marker_path(&fixture.foreign_name, "spawn_nonce");
    assert!(marker.starts_with(fixture.rig.root.path()));
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, b"opaque-before").unwrap();
    let reading = settled_owner_observation(
        "A2_STABLE_OBSERVATION superseded_retries",
        || reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel),
        |sample| sample.observed.reason == "superseded",
    )
    .await;
    assert_eq!(reading.observed, observed(Activity::Idle, "no_session"));
    let control = AtomicUsize::new(0);
    assert!(
        reading
            .publish_if_current(|| control.fetch_add(1, Ordering::SeqCst))
            .await
            .is_some()
    );
    assert_eq!(control.load(Ordering::SeqCst), 1);
    let epoch = crate::services::discord::tmux_watcher_registry::owner_observation_epoch();

    std::fs::write(&marker, b"opaque-after").unwrap();
    assert_eq!(
        crate::services::discord::tmux_watcher_registry::owner_observation_epoch(),
        epoch
    );
    for runtime in &fixture.foreign {
        assert_eq!(
            runtime
                .tmux_watchers
                .owner_channel_for_tmux_session(&fixture.foreign_name),
            Some(fixture.foreign_channel)
        );
    }
    let stale_calls = AtomicUsize::new(0);
    assert!(
        reading
            .publish_if_current(|| stale_calls.fetch_add(1, Ordering::SeqCst))
            .await
            .is_none(),
        "nonce-only change reused the old Idle"
    );
    assert_eq!(stale_calls.load(Ordering::SeqCst), 0);
    let fresh = settled_owner_observation(
        "A2_STABLE_OBSERVATION superseded_retries",
        || reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel),
        |sample| sample.observed.reason == "superseded",
    )
    .await;
    assert_eq!(fresh.observed, observed(Activity::Idle, "no_session"));
    let fresh_calls = AtomicUsize::new(0);
    assert!(
        fresh
            .publish_if_current(|| fresh_calls.fetch_add(1, Ordering::SeqCst))
            .await
            .is_some()
    );
    assert_eq!(fresh_calls.load(Ordering::SeqCst), 1);
    for bad in [&b" \n"[..], &[0xff][..]] {
        std::fs::write(&marker, bad).unwrap();
        assert_eq!(
            settled_owner_observation(
                "A2_STABLE_OBSERVATION superseded_retries",
                || reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel),
                |sample| sample.observed.reason == "superseded"
            )
            .await
            .observed,
            observed(Activity::Unknown, "session_identity_unresolved"),
            "unreadable nonce must not authorize foreign owner evidence"
        );
    }
    fixture
        .rig
        .shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&fixture.foreign_name, fixture.foreign_channel);
    assert_eq!(
        settled_owner_observation(
            "A2_STABLE_OBSERVATION superseded_retries",
            || reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel),
            |sample| sample.observed.reason == "superseded"
        )
        .await
        .observed,
        observed(Activity::Idle, "no_session"),
        "the optional nonce diagnostic must preserve existing local watcher truth"
    );
    eprintln!(
        "A2_NONCE_PUBLICATION stale_callbacks={} fresh_callbacks={}",
        stale_calls.load(Ordering::SeqCst),
        fresh_calls.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a2_foreign_receipt_only_self_change_rejects_saved_idle_publication_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_foreign_receipt_only_self_change_rejects_saved_idle_publication_pg"
    )) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_122, ProviderKind::Claude).await;
    fixture.produce_owners();
    let pool = fixture.rig.shared.pg_pool.as_ref().unwrap();
    let key = adk_session::build_namespaced_session_key(
        &fixture.foreign[0].token_hash,
        &ProviderKind::Codex,
        &fixture.foreign_name,
    );
    let initial_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM routine_runs WHERE owned_tmux_session = $1")
            .bind(&key)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(initial_rows, 0);
    let reading = settled_owner_observation(
        "A2_STABLE_OBSERVATION superseded_retries",
        || reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel),
        |sample| sample.observed.reason == "superseded",
    )
    .await;
    assert_eq!(reading.observed, observed(Activity::Idle, "no_session"));
    assert!(reading.publish_if_current(|| ()).await.is_some());
    let epoch = crate::services::discord::tmux_watcher_registry::owner_observation_epoch();
    let generation = fixture.registry.unbound_runtime_snapshot().await.generation;

    sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('a2-self-receipt', 'r.js', 'a2-self-receipt')").execute(pool).await.unwrap();
    sqlx::query("INSERT INTO routine_runs (id, routine_id, status, turn_id, result_json, owned_tmux_session, finished_at) VALUES ('a2-self-run', 'a2-self-receipt', 'succeeded', 'a2-self-turn', $1, $2, NOW())")
        .bind(serde_json::json!({"channel_id": fixture.rig.channel.to_string()}))
        .bind(&key).execute(pool).await.unwrap();
    assert_eq!(
        crate::services::discord::tmux_watcher_registry::owner_observation_epoch(),
        epoch
    );
    assert_eq!(
        fixture.registry.unbound_runtime_snapshot().await.generation,
        generation
    );
    for runtime in &fixture.foreign {
        assert_eq!(
            runtime
                .tmux_watchers
                .owner_channel_for_tmux_session(&fixture.foreign_name),
            Some(fixture.foreign_channel)
        );
    }
    let stale_calls = AtomicUsize::new(0);
    assert!(
        reading
            .publish_if_current(|| stale_calls.fetch_add(1, Ordering::SeqCst))
            .await
            .is_none(),
        "receipt-only self change reused the old Idle"
    );
    assert_eq!(stale_calls.load(Ordering::SeqCst), 0);
    let fresh = settled_owner_observation(
        "A2_STABLE_OBSERVATION superseded_retries",
        || reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel),
        |sample| sample.observed.reason == "superseded",
    )
    .await;
    assert_eq!(
        fresh.observed,
        observed(Activity::Unknown, "watcher_unbound")
    );
    eprintln!(
        "A2_RECEIPT_PUBLICATION stale_callbacks={} initial_rows={} fresh_reason={}",
        stale_calls.load(Ordering::SeqCst),
        initial_rows,
        fresh.observed.reason
    );
}
