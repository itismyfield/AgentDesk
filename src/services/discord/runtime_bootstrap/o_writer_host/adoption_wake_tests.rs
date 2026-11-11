//! The adoption wake: recovery gets only a row no live turn here holds, and an adoption commit
//! settles a row kept for it once, with no Legacy body sent.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::tui_o::cutover::test_override;
use crate::test_support::wait::{WAIT_CAP, await_within};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

const BODY: &str = "ADK-wake-kept-body";

/// A Claude TUI turn whose tmux is gone, as the boot recovery saw it: answered but not relayed.
fn kept_row(channel: u64) -> InflightTurnState {
    let tmux = format!("AgentDesk-claude-o-wake-{channel}");
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        0,
        "prompt".to_string(),
        None,
        Some(tmux),
        None,
        None,
        64,
    );
    row.full_response = BODY.to_string();
    row.runtime_kind = Some(ClaudeTui);
    inflight::save_inflight_state(&row).unwrap();
    row
}

fn watcher(tmux: &str, cancelled: bool) -> crate::services::discord::TmuxWatcherHandle {
    crate::services::discord::TmuxWatcherHandle {
        tmux_session_name: tmux.to_owned(),
        output_path: String::new(),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(cancelled)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(chrono::Utc::now().timestamp_millis())),
    }
}

/// Only an idle mailbox and no live watcher on the row's tmux leave the row to the wake.
#[tokio::test(flavor = "current_thread")]
async fn only_a_row_no_live_turn_holds_is_left_to_the_adoption_wake() {
    with_isolated_runtime_root(|| async move {
        let channel = 9_765_101;
        let row = kept_row(channel);
        let tmux = row.tmux_session_name.clone().unwrap();
        let kept = |shared: Arc<SharedData>| async move {
            kept_rows(&shared, &ProviderKind::Claude, channel)
                .await
                .len()
        };
        let cases = [
            ("no watcher", None, 1),
            (
                "another tmux",
                Some(watcher("AgentDesk-claude-other", false)),
                1,
            ),
            ("cancelled", Some(watcher(&tmux, true)), 1),
            ("live watcher", Some(watcher(&tmux, false)), 0),
        ];
        for (case, handle, expected) in cases {
            let shared = crate::services::discord::make_shared_data_for_tests();
            if let Some(handle) = handle {
                shared.tmux_watchers.insert(ChannelId::new(channel), handle);
            }
            assert_eq!(kept(shared).await, expected, "{case}");
        }
        let shared = crate::services::discord::make_shared_data_for_tests();
        let token = Arc::new(crate::services::provider::CancelToken::new());
        let (owner, message) = (serenity::UserId::new(1), serenity::MessageId::new(10));
        let claim = crate::services::discord::mailbox_try_start_turn;
        assert!(claim(&shared, ChannelId::new(channel), token, owner, message).await);
        assert_eq!(
            kept(shared).await,
            0,
            "an active mailbox turn holds the row"
        );
    })
    .await;
}

/// A row the boot kept for a pending adoption is settled by the commit's own wake alone.
#[tokio::test(flavor = "current_thread")]
async fn an_adoption_commit_settles_the_kept_row_once_without_a_legacy_body_pg() {
    with_isolated_runtime_root(|| async move {
        let provider = ProviderKind::Claude;
        let channel = 9_765_111;
        // The keyed teardown gate confirms the dead tmux against PG's sessions table.
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "adoption_wake",
            "kept row wake",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let _pending = test_override::force_candidates(&[(channel, ClaudeTui)]);
        let row = kept_row(channel);
        // The turn's tmux is gone, so recovery takes the missing-session branch.
        use crate::services::session_host::test_support::{
            InjectedLivenessGuard, InjectedPresenceGuard,
        };
        use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
        let session = HostSessionRef::tmux(row.tmux_session_name.as_deref().unwrap());
        let _dead = InjectedLivenessGuard::set(session.clone(), HostLiveness::DeadOrAbsent);
        let _gone = InjectedPresenceGuard::set(session, HostPresence::Missing);
        let recorder = o_cut_recorder::start(channel).await;
        let restore = crate::services::discord::recovery_engine::restore_inflight_states;
        restore(&recorder.http, &shared, &provider, vec![row.clone()]).await;
        let still = inflight::load_inflight_state(&provider, channel);
        assert_eq!(still.map(|row| row.recovery_relay_attempts), Some(0));
        assert!(recorder.contents().is_empty(), "{:?}", recorder.contents());

        let root = crate::config::runtime_root().unwrap();
        crate::test_support::o_store::commit_o_store(&root, &[channel]);
        let snapshot = crate::services::tui_o::cutover::boot_snapshot().unwrap();
        assert!(snapshot.candidate(channel).unwrap().confirm_store());
        let host = GatewayHost {
            shared: Arc::clone(&shared),
            execution: BTreeMap::new(),
            alarms: Arc::new(AlarmRouter::for_process(None, None)),
            self_id_wait: Duration::ZERO,
            configured_id: None,
            turn: TurnConfig::off(),
            rest: Some(Arc::clone(&recorder.http)),
            boot: None,
        };
        let began = std::time::Instant::now();
        host.adopted(channel, ShadowProvider::Claude);
        let settled = async {
            while inflight::load_inflight_state(&provider, channel).is_some() {
                tokio::task::yield_now().await;
            }
        };
        await_within(settled, WAIT_CAP).await;
        eprintln!("WAKE_SETTLED_AFTER {:?}", began.elapsed());
        // Legacy shows only the body-free recovery marker; the body is O's to post.
        let contents = recorder.contents();
        assert_eq!(contents.len(), 1, "{contents:?}");
        assert!(!contents[0].contains(BODY), "{contents:?}");
        pool.close().await;
        db.drop().await;
    })
    .await;
}
