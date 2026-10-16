//! Legacy restore effects are observed through the public entry in a child with fake tmux.
use super::super::watcher_install_fixture::{fixture_with_pool, isolated, list_sessions};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::legacy_supervision::test_support;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::task_supervisor::watcher_spawn_observation::Scope;
use crate::services::tmux_common as tc;
use std::path::{Path, PathBuf};

fn seed_row(fixture: &InstallFixture) -> PathBuf {
    let mut row = InflightTurnState::new(
        fixture.provider.clone(),
        fixture.channel.get(),
        Some("n3bc-old".into()),
        7,
        111,
        112,
        "old prompt".into(),
        Some("old-native".into()),
        Some(fixture.session.clone()),
        Some(fixture.output.clone()),
        None,
        0,
    );
    row.turn_start_offset = Some(0);
    row.turn_nonce = Some(format!("n3bc-nonce-{}", fixture.channel.get()));
    row.session_key = Some(
        crate::services::discord::adk_session::build_namespaced_session_key(
            &fixture.shared.token_hash,
            &fixture.provider,
            &fixture.session,
        ),
    );
    test_support::seed_backfill_row(&row)
}

async fn pair(
    pool: &sqlx::PgPool,
    channel: u64,
) -> (InstallFixture, InstallFixture, test_support::MockDiscord) {
    let (protected, discord) =
        fixture_with_pool(ProviderKind::Claude, channel, Some(pool.clone())).await;
    let mut sibling =
        InstallFixture::new_with_pool(ProviderKind::Claude, channel + 1, Some(pool.clone())).await;
    sibling.shared = protected.shared.clone();
    sibling.registry = protected.registry.clone();
    sibling.http = protected.http.clone();
    protected.shared.settings.write().await.allowed_channel_ids = vec![channel, channel + 1];
    for fixture in [&protected, &sibling] {
        std::fs::write(tc::session_temp_path(&fixture.session, "host_kind"), "tmux").unwrap();
        std::fs::write(tc::session_temp_path(&fixture.session, "generation"), "4").unwrap();
        test_support::age_file(
            Path::new(&tc::session_temp_path(&fixture.session, "generation")),
            3600,
        );
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &fixture.shared.token_hash,
            &fixture.provider,
            &fixture.session,
        );
        crate::services::discord::inflight::seed_session_row_hashed(
            pool,
            &key,
            fixture.channel.get(),
            &fixture.shared.token_hash,
            None,
        )
        .await;
    }
    list_sessions(&[&protected.session, &sibling.session]);
    (protected, sibling, discord)
}

#[tokio::test]
async fn n3bc_restore_off_registry_keeps_legacy_adoption_and_old_row_semantics_pg() {
    if !isolated(
        module_path!(),
        "n3bc_restore_off_registry_keeps_legacy_adoption_and_old_row_semantics_pg",
    ) {
        return;
    }
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(20));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (legacy, sibling, _discord) = pair(&pool, 632_532_200).await;
    let row = seed_row(&legacy);
    let generation = PathBuf::from(tc::session_temp_path(&legacy.session, "generation"));
    let old_mtime = std::fs::metadata(&generation).unwrap().modified().unwrap();
    std::fs::write(
        tc::session_dead_marker_path(&legacy.session),
        "stale live-pane tombstone",
    )
    .unwrap();
    let row_before = test_support::fingerprint(&row).unwrap();
    let submitted = Scope::new(&legacy.shared);
    restore_tmux_watchers(&legacy.http, &legacy.shared).await;
    assert_eq!(std::fs::read_to_string(&generation).unwrap(), "20");
    assert_eq!(
        std::fs::metadata(&generation).unwrap().modified().unwrap(),
        old_mtime,
        "old numeric generation adoption preserves wrapper identity mtime"
    );
    assert!(
        !Path::new(&tc::session_dead_marker_path(&legacy.session)).exists(),
        "live pane clears the old marker off"
    );
    assert_ne!(
        test_support::fingerprint(&row).unwrap(),
        row_before,
        "old row is compatibility-backfilled and readopted off"
    );
    let mailbox = legacy
        .shared
        .mailbox_peek(legacy.channel)
        .unwrap()
        .snapshot()
        .await;
    assert!(
        mailbox.cancel_token.is_some() && mailbox.active_user_message_id.is_some(),
        "Legacy row registers its mailbox off"
    );
    for fixture in [&legacy, &sibling] {
        assert!(
            legacy
                .shared
                .core
                .lock()
                .await
                .sessions
                .contains_key(&fixture.channel)
        );
        assert!(legacy.shared.tmux_watchers.contains_key(&fixture.channel));
    }
    assert_eq!(submitted.submissions(), 2);
    submitted.cancel_and_join_all().await;
    pool.close().await;
    db.drop().await;
}
