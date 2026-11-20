//! A boot restore and an adoption wake reaching one kept row: the second waits for the first
//! and finds the row settled, so the recovery marker is posted once.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::recovery_engine::o_cut_recorder::{self, FirstPost};
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::tui_o::cutover::test_override::force_channels;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[tokio::test(flavor = "current_thread")]
async fn a_second_restore_of_a_kept_row_waits_for_the_first_and_posts_nothing_pg() {
    use crate::services::session_host::test_support::{
        InjectedLivenessGuard, InjectedPresenceGuard,
    };
    use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
    with_isolated_runtime_root(|| async move {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "restore_serial",
            "serial restore",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let (provider, channel) = (ProviderKind::Claude, 9_765_301u64);
        let _owned = force_channels(&[(channel, ClaudeTui)]);
        let tmux = format!("AgentDesk-claude-o-serial-{channel}");
        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            7,
            channel + 1,
            0,
            "prompt".to_string(),
            None,
            Some(tmux.clone()),
            None,
            None,
            64,
        );
        row.full_response = "ADK-serial-kept-body".to_string();
        row.runtime_kind = Some(ClaudeTui);
        inflight::save_inflight_state(&row).unwrap();
        let session = HostSessionRef::tmux(&tmux);
        let _dead = InjectedLivenessGuard::set(session.clone(), HostLiveness::DeadOrAbsent);
        let _gone = InjectedPresenceGuard::set(session, HostPresence::Missing);
        let first = FirstPost::holding();
        let recorder = o_cut_recorder::start_first_post(channel, Arc::clone(&first)).await;
        let run = |row: inflight::InflightTurnState| {
            let (http, shared, provider) =
                (recorder.http.clone(), shared.clone(), provider.clone());
            tokio::spawn(
                async move { restore_serially(&http, &shared, &provider, vec![row]).await },
            )
        };
        let boot = run(row.clone());
        await_within(first.arrived.notified(), WAIT_CAP).await;
        let wake = run(row.clone());
        // The second run either waits on the channel's lock or, without one, posts its own marker.
        let lock = || {
            shared
                .restart
                .restore_locks
                .get(&channel)
                .map(|l| Arc::strong_count(&l))
        };
        let parked = async {
            while first.posts() < 2 && lock().is_none_or(|holders| holders < 3) {
                tokio::task::yield_now().await;
            }
        };
        await_within(parked, WAIT_CAP).await;
        first.release.add_permits(1);
        await_within(boot, WAIT_CAP).await.unwrap();
        await_within(wake, WAIT_CAP).await.unwrap();
        assert_eq!(first.posts(), 1, "{:?}", recorder.contents());
        assert!(inflight::load_inflight_state(&provider, channel).is_none());
        pool.close().await;
        db.drop().await;
    })
    .await;
}
