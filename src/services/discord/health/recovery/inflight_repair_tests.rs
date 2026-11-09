use poise::serenity_prelude::ChannelId;

use crate::config::TestEnvVarGuard;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::recovery::stall_watchdog_auto_heal_tests::{
    result_line, seed_idle_inflight,
};
use crate::services::discord::relay_recovery::tests::orphan_token_finish::v2_attempted_front;
use crate::services::provider::ProviderKind;

// With no inflight user id to finish by, the repair still reports the input a V2 owner has
// waiting; a front an attempt carries is not waiting.
#[tokio::test(flavor = "current_thread")]
async fn unpinned_stale_turn_repair_reports_the_v2_owner_waiting_input() {
    let tempdir = tempfile::tempdir().expect("runtime root tempdir");
    let _env = TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tempdir.path());
    let provider = ProviderKind::Claude;
    let registry = HealthRegistry::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let output = tempdir.path().join("unpinned-stale-turn.jsonl");
    std::fs::write(&output, result_line("")).expect("write ready output fixture");
    for (channel, waiting) in [(4_800_801, true), (4_800_802, false)] {
        let channel = ChannelId::new(channel);
        let tmux = format!("AgentDesk-claude-unpinned-{channel}");
        v2_attempted_front(&shared, &provider, channel, waiting).await;
        seed_idle_inflight(&provider, channel, 0, &tmux, &output, 0, "unpinned");

        let repair = super::super::clear_idle_tmux_stale_turn(
            &registry,
            provider.as_str(),
            channel.get(),
            &tmux,
            "unpinned_stale_turn_test",
        );
        let result = repair.await.expect("the unpinned repair runs");
        assert!(!result.had_active_turn);
        assert_eq!(result.has_pending_queue, waiting);
    }
}
