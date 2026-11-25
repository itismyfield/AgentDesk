//! The dead-Bound recovery's own judgements: the owner is compared with the asking provider, a
//! held transition lock or any turn, inflight row or unread inflight file leaves the row for a later
//! look, and a person's own intake guard never blocks it.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedObservation, HostedOwner, SourceRef};
use crate::services::herdr_launch::HerdrLaunchEndpoint;
use crate::services::provider::CancelToken;
use crate::services::turn_host::HerdrTurnPlan;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::{MessageId, UserId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Hook = Box<dyn FnOnce() + Send>;
static BEFORE_CAS: Mutex<Option<HashMap<u64, Hook>>> = Mutex::new(None);

/// Runs `hook` once inside the recovery's lease, after its no-turn reads and before its CAS.
pub(in crate::services::discord) fn install_before_cas(
    channel: u64,
    hook: impl FnOnce() + Send + 'static,
) {
    let mut hooks = BEFORE_CAS.lock().unwrap_or_else(|e| e.into_inner());
    hooks
        .get_or_insert_with(HashMap::new)
        .insert(channel, Box::new(hook));
}

pub(super) fn before_cas(channel: u64) {
    let hook = BEFORE_CAS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|hooks| hooks.remove(&channel));
    if let Some(hook) = hook {
        hook();
    }
}

const NODE: &str = "mac-mini";
const SOCKET: &str = "/tmp/i8422-herdr.sock";

fn plan_and_record(provider: &str, channel: u64) -> (HerdrTurnPlan, HostedExecution) {
    let owner = HostedOwner {
        provider: provider.into(),
        discord_token_hash: "fixture".into(),
        channel_id: channel.to_string(),
        logical_key: "AgentDesk-dead-bound".into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    let nonce = "0123456789abcdef0123456789abcdef";
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: "mini".into(),
            socket_addr: SOCKET.into(),
            named_session: "agentdesk".into(),
            pane_id: "w1-1".into(),
        }),
        expected: None,
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: owner.provider.clone(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    };
    let plan = HerdrTurnPlan {
        endpoint: HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: "mini".into(),
            socket_addr: SOCKET.into(),
            herdr_session: "agentdesk".into(),
        },
        row: Some(HostedObservation::known_fixture(record.clone())),
        bootstrap: false,
    };
    (plan, record)
}

#[test]
fn the_owner_is_compared_with_the_asking_provider_never_a_claude_constant() {
    let channel = ChannelId::new(8_422_001);
    let root = std::path::Path::new("/adk/runtime");
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let (plan, record) = plan_and_record(provider.as_str(), channel.get());
        let at = record.location.clone().unwrap();
        let ids = (&provider, "fixture", channel, "AgentDesk-dead-bound");
        assert!(
            !owner_differs((&record, &at), &plan, ids, (root, NODE)),
            "{provider:?}: its own execution is recoverable"
        );
        let other = match provider {
            ProviderKind::Claude => ProviderKind::Codex,
            _ => ProviderKind::Claude,
        };
        let ids = (&other, "fixture", channel, "AgentDesk-dead-bound");
        assert!(
            owner_differs((&record, &at), &plan, ids, (root, NODE)),
            "{provider:?}: another provider's execution is never this one's"
        );
        let mut retired = record.clone();
        retired.state = HostedState::Retired;
        let ids = (&provider, "fixture", channel, "AgentDesk-dead-bound");
        assert!(owner_differs((&retired, &at), &plan, ids, (root, NODE)));
        let ids = (&provider, "other-token", channel, "AgentDesk-dead-bound");
        assert!(owner_differs((&record, &at), &plan, ids, (root, NODE)));
    }
}

#[tokio::test]
async fn a_taken_transition_lock_leaves_the_row_and_a_borrowed_guard_does_not() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_422_002);
    assert!(exclusion(&shared, channel, LeaseRef::Acquire).is_ok());
    let held = shared
        .session_transition_locks
        .lock(channel)
        .try_lock_owned()
        .unwrap();
    assert!(matches!(
        exclusion(&shared, channel, LeaseRef::Acquire),
        Err(DeadBound::Reobserve(reason)) if reason == "session transition busy"
    ));
    // A person's intake already holds this exact lock and lends it.
    assert!(exclusion(&shared, channel, LeaseRef::Borrow(&held)).is_ok());
    let other = shared
        .session_transition_locks
        .lock(ChannelId::new(8_422_003))
        .try_lock_owned()
        .unwrap();
    assert!(matches!(
        exclusion(&shared, channel, LeaseRef::Borrow(&other)),
        Err(DeadBound::Reobserve(_))
    ));
    drop(held);
    assert!(exclusion(&shared, channel, LeaseRef::Acquire).is_ok());
}

fn inflight_file(channel: u64) -> Option<std::path::PathBuf> {
    let name = format!("{channel}.json");
    let mut dirs = vec![crate::config::runtime_root()?];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|file| file == name.as_str())
                && path.to_string_lossy().contains("inflight")
            {
                return Some(path);
            }
        }
    }
    None
}

#[tokio::test]
async fn any_turn_token_inflight_row_or_unread_inflight_file_leaves_the_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(8_422_004);
    let provider = ProviderKind::Claude;
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(channel);
    assert!(no_turn(&provider, channel, &mailbox).await.is_ok());

    let state = crate::services::discord::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        Some("dead-bound-holder".into()),
        7,
        8_422_005,
        0,
        "a watcher-owned stream".into(),
        None,
        Some("AgentDesk-dead-bound".into()),
        None,
        None,
        0,
    );
    crate::services::discord::inflight::save_inflight_state(&state).unwrap();
    assert!(matches!(
        no_turn(&provider, channel, &mailbox).await,
        Err(DeadBound::Reobserve(reason)) if reason == "an inflight row holds the channel"
    ));
    // A failed read is never an absent row.
    let path = inflight_file(channel.get()).expect("the saved inflight row");
    std::fs::write(&path, "{").unwrap();
    assert!(matches!(
        no_turn(&provider, channel, &mailbox).await,
        Err(DeadBound::Reobserve(reason)) if reason.starts_with("inflight unread")
    ));
    std::fs::remove_file(&path).unwrap();
    assert!(no_turn(&provider, channel, &mailbox).await.is_ok());

    mailbox
        .restore_active_turn(
            Arc::new(CancelToken::new()),
            UserId::new(7),
            MessageId::new(8_422_006),
        )
        .await;
    assert!(matches!(
        no_turn(&provider, channel, &mailbox).await,
        Err(DeadBound::Reobserve(reason)) if reason == "a turn holds the channel"
    ));
}

#[test]
fn an_unproven_or_running_read_is_reobserved_and_a_missing_row_is_refused() {
    assert!(matches!(
        classify(RetireRefusal::ProviderRunning),
        DeadBound::Reobserve(_)
    ));
    assert!(matches!(
        classify(RetireRefusal::Unproven("pane unreadable")),
        DeadBound::Reobserve(_)
    ));
    assert!(matches!(
        classify(RetireRefusal::Changed("stale".into())),
        DeadBound::Retry(_)
    ));
    assert!(matches!(
        classify(RetireRefusal::NoRow("unreadable record")),
        DeadBound::Refused(_)
    ));
}
