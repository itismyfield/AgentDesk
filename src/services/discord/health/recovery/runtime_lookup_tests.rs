use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{
    HardStopRuntimeResult, stop_providerless_runtime_turn_preserving_watcher_strict_ownership,
    stop_runtime_turn_preserving_watcher,
};
use crate::config::TestEnvVarGuard;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::recovery::hard_stop_completion_event_tests::capture_warns_async;
use crate::services::discord::relay_recovery::tests::orphan_token_finish::v2_attempted_front;
use crate::services::provider::ProviderKind;

// A V2 owner's waiting input is backlog the unresolved fallback warns of and ownership a hard stop
// resolves past an empty sibling, leaving the input; a front an attempt carries is neither.
#[tokio::test(flavor = "current_thread")]
async fn hard_stop_lookup_counts_the_v2_owner_waiting_input() {
    let tempdir = tempfile::tempdir().expect("runtime root tempdir");
    let _env = TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tempdir.path());
    let provider = ProviderKind::Claude;
    for (channel, waiting) in [(4_800_901, true), (4_800_902, false)] {
        let channel = ChannelId::new(channel);
        let sibling = crate::services::discord::make_shared_data_for_tests();
        let owner = crate::services::discord::make_shared_data_for_tests();
        v2_attempted_front(&owner, &provider, channel, waiting).await;
        let registry = HealthRegistry::new();
        registry
            .register(provider.as_str().to_string(), Arc::clone(&sibling))
            .await;

        let (unresolved, logs) = capture_warns_async(|| {
            let source = "v2_unresolved_test";
            let name = Some(provider.as_str());
            stop_runtime_turn_preserving_watcher(
                Some(&registry),
                name,
                Some(channel.get()),
                None,
                source,
            )
        })
        .await;
        assert_eq!(unresolved, HardStopRuntimeResult::default());
        assert_eq!(logs.contains("has_pending=true"), waiting, "{logs}");

        registry
            .register(provider.as_str().to_string(), Arc::clone(&owner))
            .await;
        let stop = stop_providerless_runtime_turn_preserving_watcher_strict_ownership;
        let resolved = stop(Some(&registry), channel.get(), "v2_owner_test").await;
        assert_eq!(
            resolved != HardStopRuntimeResult::default(),
            waiting,
            "{resolved:?}"
        );
        assert!(sibling.mailbox_peek(channel).is_none());
        let after = owner.mailbox(channel).snapshot().await;
        assert_eq!(after.queue_depth(), usize::from(waiting));
        assert!(after.holds_input(), "the hard stop keeps the owner's input");
    }
}
