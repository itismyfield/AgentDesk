use super::*;
use crate::services::discord::{
    health::{self as turn_deliver, HealthRegistry, HumanInputRequest},
    turn_presence::{activity, supervisor},
};
use crate::services::platform::tmux::session_inventory::tests::ProbeTrace;
use poise::serenity_prelude::ChannelId;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

static TRACE: std::sync::LazyLock<std::sync::Mutex<BTreeMap<u64, Arc<AtomicUsize>>>> =
    std::sync::LazyLock::new(Default::default);
pub(super) fn reached(channel: u64) {
    if let Some(count) = TRACE.lock().unwrap().get(&channel) {
        count.fetch_add(1, Ordering::SeqCst);
    }
}
struct Trace(u64, Arc<AtomicUsize>);
impl Trace {
    fn new(channel: u64) -> Self {
        let count = Arc::new(AtomicUsize::new(0));
        assert!(
            TRACE
                .lock()
                .unwrap()
                .insert(channel, count.clone())
                .is_none()
        );
        Self(channel, count)
    }
    fn calls(&self) -> usize {
        self.1.load(Ordering::SeqCst)
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        TRACE.lock().unwrap().remove(&self.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn activation_candidates_counter_crosses_the_blocking_worker() {
    const CHANNEL: u64 = 69_181_997;
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let observer = Trace::new(CHANNEL);
    let ports = activity::LivePorts {
        root: Some(root.path().to_owned()),
    };
    let candidates = tokio::task::spawn_blocking(move || {
        ports.scoped(|| {
            let log = crate::services::tui_o::writer::binding::ChannelBindingLog::new(
                CHANNEL,
                ShadowProvider::Claude,
            );
            observe_candidates(
                CHANNEL,
                &ProviderKind::Claude,
                Some("worker-positive"),
                &[],
                &log,
            )
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert!(candidates.current.is_some());
    assert_eq!(
        observer.calls(),
        1,
        "worker collection was invisible to its positive counter"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn u_dormant_entrypoints() {
    const CHANNEL: u64 = 69_180_099;
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = activity::tests::BindingRoot::enter(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(CHANNEL);
    let _turn_mode = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let observer = Trace::new(CHANNEL);
    let inventory = ProbeTrace::new();
    assert!(shared.tmux_watchers.channel_binding(&channel).is_none());
    assert_eq!(
        activity::activity_now(&shared, &provider, channel)
            .await
            .reason,
        "no_session"
    );
    assert!(
        crate::services::discord::input_runtime::fence::effect::admit(&provider, CHANNEL).is_ok(),
        "the automatic entry must pass its admission gate"
    );
    let selected = crate::services::discord::mailbox_take_next_automatic_intervention(
        &shared, &provider, channel,
    )
    .await;
    assert!(selected.intervention.is_none());
    let registry = HealthRegistry::new();
    turn_deliver::set_human_input_auth_for_tests(&shared, Some(1), vec![], false).await;
    {
        shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
    }
    registry.register("claude".into(), shared.clone()).await;
    let delivered = turn_deliver::deliver_human_input(
        &registry,
        HumanInputRequest {
            channel_id: channel,
            provider: provider.clone(),
            text: "dormant fixture".into(),
            author_id: 1,
            source: "test".into(),
            metadata: None,
            channel_name_hint: None,
        },
    )
    .await;
    assert!(
        matches!(delivered, Err(turn_deliver::HumanInputError::RuntimeUnavailable(ref detail)) if detail == "provider runtime is not ready"),
        "the real external-hold read must precede the start refusal: {delivered:?}"
    );
    assert_eq!(
        turn_deliver::external_turn_hold_for_start(Some(&registry), &provider, CHANNEL).await,
        None
    );
    supervisor::register(&provider, &[CHANNEL]);
    supervisor::spawn(shared.clone(), provider.clone());
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(status) = supervisor::status(CHANNEL) {
                if status["reason"] == "no_session" {
                    break status;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the real supervisor must publish its read_now result");
    assert_eq!(status["reason"], "no_session");
    assert!(!status["observed_at"].is_null());
    assert_eq!(observer.calls(), 0);
    assert_eq!(inventory.calls(), 0);
    assert!(
        unbound_candidates(
            CHANNEL,
            &provider,
            Some("positive-control"),
            &[],
            Ok(vec![]),
            &Ok(None)
        )
        .is_ok()
    );
    assert_eq!(
        observer.calls(),
        1,
        "the counter must observe its own positive control"
    );
    let result = crate::services::platform::tmux::session_inventory::tests::probe_in_namespace(
        &root.path().join("never-created"),
    );
    assert!(matches!(result, Inventory::Empty(_)), "{result:?}");
    assert_eq!(
        inventory.calls(),
        1,
        "the inventory entry counter must be live"
    );
}
