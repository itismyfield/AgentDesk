//! A paused turn attach on a Herdr pane skips the tmux wait and commits only under the
//! reconcile's admission; a withheld attach leaves the inflight row and the registry as they were.

use super::cold_start_retry_tests::RetryTestGuard;
use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, seed, shared_on,
};
use crate::services::tmux_common::session_temp_path;
use crate::services::tui_prompt_dedupe::{admit_herdr_execution, install_herdr_execution};

fn http() -> Arc<serenity::Http> {
    Arc::new(serenity::Http::new("Bot test-token"))
}

fn mark_herdr(name: &str) {
    let marker = session_temp_path(name, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
}

// The turn-start attach on a pane only its sessions row puts on Herdr, never admitted, sets no
// owner, saves nothing and schedules no tmux retry; a legacy row still defers to tmux as before.
#[tokio::test]
async fn a_withheld_paused_attach_leaves_the_row_registry_and_retries_untouched_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _retry = RetryTestGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let root = crate::services::discord::inflight::inflight_runtime_root().unwrap();
    // (row, channel, attach result, tmux retries scheduled)
    let cases = [
        (Stored::Hosted, 5340_31, false, 0),
        (Stored::Legacy, 5340_32, true, 1),
    ];
    for (stored, channel, attached, retries) in cases {
        let name = format!("AgentDesk-claude-p8c6-{channel}");
        let output = format!("/tmp/{name}.jsonl");
        seed(&pool, &channel_key(&shared, &name), &name, channel, stored).await;
        let (claude, tmux) = (ProviderKind::Claude, Some(name.clone()));
        let (text, out) = ("p8".to_string(), Some(output.clone()));
        let mut row = InflightTurnState::new(
            claude.clone(),
            channel,
            None,
            1,
            2,
            3,
            text,
            None,
            tmux,
            out,
            None,
            0,
        );
        row.watcher_owner_channel_id = None;
        crate::services::discord::inflight::save_inflight_state_create_new(&row).unwrap();
        let path = crate::services::discord::inflight::inflight_state_path(&root, &claude, channel);
        let before = std::fs::read(&path).unwrap();
        let channel_id = serenity::ChannelId::new(channel);
        let owner = attach_paused_turn_watcher_for_inflight(
            &shared,
            http(),
            &claude,
            channel_id,
            Some(name.clone()),
            Some(output),
            0,
            "turn_start_message",
            None,
            &mut row,
        )
        .await;
        let pending = pending_paused_watcher_attach_count_for_tests();
        clear_pending_paused_watcher_attaches_for_tests();
        assert_eq!(
            owner,
            attached.then_some(channel_id),
            "{stored:?}: attach result"
        );
        let recorded = attached.then_some(channel);
        assert_eq!(
            row.watcher_owner_channel_id, recorded,
            "{stored:?}: owner setter"
        );
        let kept = std::fs::read(&path).unwrap() == before;
        assert!(
            attached || kept,
            "{stored:?}: a withheld attach saves nothing"
        );
        assert_eq!(pending, retries, "{stored:?}: tmux retries scheduled");
        assert!(
            !shared.tmux_watchers.contains_key(&channel_id),
            "{stored:?}"
        );
    }
    pool.close().await;
    db.drop().await;
}

// A retry scheduled on a legacy snapshot re-reads the host each attempt: an admitted Herdr launch
// attaches without waiting for tmux, and a marked pane no reconcile admitted is withheld.
#[tokio::test(start_paused = true)]
async fn a_scheduled_paused_attach_rereads_the_host_before_each_attempt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _retry = RetryTestGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let [admitted, withheld] = [
        "AgentDesk-claude-p8c6-admitted",
        "AgentDesk-claude-p8c6-withheld",
    ];
    let channels = [5340_41, 5340_42].map(serenity::ChannelId::new);
    for (name, channel) in [admitted, withheld].into_iter().zip(channels) {
        let output = Some(format!("/tmp/{name}.jsonl"));
        let owner = attach_paused_turn_watcher(
            &shared,
            http(),
            &ProviderKind::Claude,
            channel,
            Some(name.to_string()),
            output,
            0,
            "turn_start_message",
            None,
        );
        assert_eq!(owner, channel, "{name}: deferred on the legacy snapshot");
    }
    assert_eq!(pending_paused_watcher_attach_count_for_tests(), 2);

    // Before any retry runs: both panes move to Herdr; only the withheld one also looks live on tmux.
    mark_herdr(admitted);
    install_herdr_execution(admitted, "p8-n1");
    admit_herdr_execution(admitted, "p8-n1");
    mark_herdr(withheld);
    set_test_paused_watcher_tmux_live_override(Some(&[withheld]));
    while pending_paused_watcher_attach_count_for_tests() != 0 {
        tokio::time::sleep(PAUSED_WATCHER_COLD_START_RETRY_DELAY).await;
    }

    let attached = shared
        .tmux_watchers
        .get(&channels[0])
        .map(|w| w.tmux_session_name.clone());
    assert_eq!(
        attached.as_deref(),
        Some(admitted),
        "admitted Herdr pane attaches"
    );
    assert!(
        !shared.tmux_watchers.contains_key(&channels[1]),
        "marked pane is withheld"
    );
}
