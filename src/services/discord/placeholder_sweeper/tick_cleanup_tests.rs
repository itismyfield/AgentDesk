use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{StalledEditTracker, run_placeholder_sweeper_tick};
use crate::config::TestEnvVarGuard;
use crate::services::discord::abandon_request_store::{
    self, AbandonEpisodeIdentity, AbandonRecord,
};
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::MockDiscord;
use crate::services::discord::health::transcript_turn::tests::project_for_test;
use crate::services::discord::{SharedData, runtime_store, status_panel_orphan_store};
use crate::services::provider::ProviderKind;

const PROVIDER: ProviderKind = ProviderKind::Codex;

fn abandon(msg_id: u64) -> AbandonRecord {
    AbandonRecord {
        msg_id,
        started_at: "2026-10-01 00:00:00".into(),
        current_tool_line: None,
        terminal_status: Default::default(),
        episode: AbandonEpisodeIdentity {
            user_msg_id: 77,
            started_at: "2026-10-01 00:00:00".into(),
            ..Default::default()
        },
    }
}

fn shared_with_token(token: &str) -> Arc<SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().token_hash = token.to_string();
    shared
}

/// Token-scoped cleanup records stay visible until every runtime's own tick freshly confirms
/// its store empty; a busy binding alone is cleanup detail, not residue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_cleanup_detail_needs_every_runtime_to_confirm_absence() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
    let discord = MockDiscord::start().await;
    let (a, b) = (shared_with_token("n4b-a"), shared_with_token("n4b-b"));
    let tokens = [a.token_hash.clone(), b.token_hash.clone()];
    let (channel, busy_channel) = (6_325_400_401u64, 6_325_400_402u64);
    let _retired = [channel, busy_channel].map(|c| RetiredForTest::new("codex", c));
    status_panel_orphan_store::enqueue(&PROVIDER, &a.token_hash, channel, channel * 10 + 1);
    abandon_request_store::enqueue(&PROVIDER, &a.token_hash, channel, abandon(channel * 10 + 2))
        .unwrap();
    let busy = runtime_store::discord_busy_followup_retries_root()
        .unwrap()
        .join(format!("codex/{busy_channel}"));
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("5.json"), "{}").unwrap();
    let mut tracker = StalledEditTracker::default();
    let cleanup = |now: Instant| {
        let (reasons, health) = project_for_test("codex", channel, &tokens, now);
        assert!(
            reasons.iter().all(|r| !r.contains("legacy_residue:")),
            "{reasons:?}"
        );
        (
            health["legacy_cleanup_pending"].clone(),
            health["unobserved"].clone(),
        )
    };
    let pending = serde_json::json!({"orphan_panel": 1, "abandon_request": 1});

    run_placeholder_sweeper_tick(&discord.http, &a, &PROVIDER, &mut tracker).await;
    run_placeholder_sweeper_tick(&discord.http, &b, &PROVIDER, &mut tracker).await;
    let (detail, unobserved) = cleanup(Instant::now());
    assert_eq!(detail, pending);
    assert!(
        !unobserved.to_string().contains("orphan_panel"),
        "{unobserved}"
    );

    let runtime = runtime_store::runtime_root().unwrap();
    for dir in ["discord_status_panel_orphans", "discord_abandon_requests"] {
        std::fs::remove_dir_all(runtime.join(dir)).unwrap();
    }
    run_placeholder_sweeper_tick(&discord.http, &b, &PROVIDER, &mut tracker).await;
    assert_eq!(
        cleanup(Instant::now()).0,
        pending,
        "b cannot release a's records"
    );
    run_placeholder_sweeper_tick(&discord.http, &a, &PROVIDER, &mut tracker).await;
    let now = Instant::now();
    let (detail, unobserved) = cleanup(now);
    assert_eq!(detail, serde_json::json!({}));
    assert!(
        !unobserved.to_string().contains("orphan_panel"),
        "{unobserved}"
    );
    let stale = cleanup(now + Duration::from_secs(91)).1.to_string();
    assert!(
        stale.contains("orphan_panel") && stale.contains("abandon_request"),
        "{stale}"
    );

    let (reasons, busy_only) = project_for_test("codex", busy_channel, &tokens, now);
    assert!(
        reasons.iter().all(|r| !r.contains("legacy_residue")),
        "{reasons:?}"
    );
    assert_eq!(
        busy_only["legacy_cleanup_pending"],
        serde_json::json!({"busy_retry": 1})
    );
}
