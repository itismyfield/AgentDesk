use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId};

use super::super::clear_persist_failure_tests::{
    SESSION_ID, boundary_rows, queue_len, seed_backlog, seed_session, session_state,
};
use super::super::{SoftClearNotifyMode, clear_channel_session_state};
use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::provider::ProviderKind;

/// What a refused clear must have left alone: the queued backlog, the provider session, the
/// transcript boundary and the session transition.
async fn untouched(shared: &Arc<crate::services::discord::SharedData>, pool: &sqlx::PgPool) {
    let channel_id = ChannelId::new(CHANNEL);
    assert_eq!(queue_len(shared, channel_id).await, 1);
    let session = session_state(shared, channel_id).await;
    assert_eq!(session, (Some(SESSION_ID.to_string()), false));
    assert!(boundary_rows(pool, channel_id).await.is_empty());
    let transition = shared.session_transition_lock(channel_id).try_lock_owned();
    assert!(transition.is_ok(), "no transition was taken");
}

const CHANNEL: u64 = 9_200_000_000_000_101;

// The `/clear` and `!clear` entry: refused before any change while the home drains or is not held
// here; once no home is registered for the channel the clear runs as before.
#[test]
fn a_delegated_channel_clear_is_refused_unless_its_home_is_held_with_intake_open_pg() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel_id = ChannelId::new(CHANNEL);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_clear_home_fence",
            "delegated channel clear fence",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        seed_session(&shared, channel_id).await;
        seed_backlog(&shared, &provider, channel_id).await;
        let http = Arc::new(serenity::Http::new(""));
        let clear = || {
            let source = "!clear";
            let notify = SoftClearNotifyMode::Suppress;
            clear_channel_session_state(&http, &shared, &provider, channel_id, source, notify)
        };
        let refused = |result: anyhow::Result<()>| {
            let error = result.expect_err("refused");
            *error
                .downcast_ref::<CommandRefused>()
                .expect("typed refusal")
        };

        let home = channel_home::register_for_test(CHANNEL, Some(HomeState::Releasing));
        let draining = refused(clear().await);
        assert_eq!(draining, CommandRefused(HomeRefusal::Draining));
        untouched(&shared, &pool).await;
        home.close();
        let not_held = refused(clear().await);
        assert_eq!(not_held, CommandRefused(HomeRefusal::NotHeld));
        untouched(&shared, &pool).await;

        channel_home::register_for_test(CHANNEL + 1, Some(HomeState::Worker));
        channel_home::unregister(&CHANNEL.to_string());
        let cleared = clear().await;
        assert!(cleared.is_ok(), "{cleared:?}");
        assert_eq!(queue_len(&shared, channel_id).await, 0);
        assert_eq!(session_state(&shared, channel_id).await, (None, true));
        assert_eq!(boundary_rows(&pool, channel_id).await, [1]);

        pool.close().await;
        db.drop().await;
    });
}
