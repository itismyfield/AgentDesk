//! Confirmed absence ends a working-turn watch; an observation without context is never watched.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedObservation, HostedOwner, HostedState, SourceRef,
};
use crate::services::herdr_launch::HerdrLaunchEndpoint;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::turn_host::{HerdrRefusal, HerdrTurnPlan};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::{MessageId, UserId};

#[tokio::test]
async fn an_observation_without_a_context_has_no_working_turn_watch() {
    let host = TurnHost::Refused(HerdrRefusal::OWriterNotReady);
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(ChannelId::new(8212203));
    assert!(
        recoverable_probe(
            &host,
            &BusyObservation::only(BusyState::Held),
            None,
            &mailbox,
            None
        )
        .is_none()
    );
}

#[tokio::test]
async fn a_confirmed_missing_working_pane_ends_its_watch_without_dropping_the_front() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(8212002);
    let logical = "AgentDesk-claude-missing-probe";
    let nonce = "0123456789abcdef0123456789abcdef";
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    rig.run_provider(&rig.context(nonce), false);
    rig.show_panes(&[PANE]);
    let marker = crate::services::tmux_common::session_temp_path(logical, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "fixture".into(),
        channel_id: channel.get().to_string(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
    };
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(nonce)),
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
            config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        },
        row: Some(HostedObservation::known_fixture(record)),
        bootstrap: false,
    };
    let target = claude_handoff::bound_herdr(&plan).unwrap();
    let observation = crate::services::claude_tui::host_input::busy::observe_herdr(
        (channel.get(), logical, nonce),
        target,
        |_| false,
    );
    let host = TurnHost::Herdr(Box::new(plan));
    rig.show_panes(&[]);
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(channel);
    mailbox
        .v2_adopt(
            crate::config::runtime_root()
                .unwrap()
                .join("runtime/working-probe.json"),
            "probe".into(),
        )
        .await
        .unwrap();
    let item:PendingQueueItem = serde_json::from_value(json!({"author_id":7,"message_id":1,"channel_id":channel.get(),"text":"working followup","queued_generation":1})).unwrap();
    mailbox
        .v2_admit_ticket(item, "working-probe".into())
        .await
        .unwrap();
    mailbox
        .restore_active_turn(
            std::sync::Arc::new(CancelToken::new()),
            UserId::new(7),
            MessageId::new(2),
        )
        .await;
    let front = mailbox
        .v2_request(|slot| Ok(slot.as_ref().unwrap().front().unwrap()))
        .await
        .unwrap();
    let probe = recoverable_probe(&host, &observation, None, &mailbox, Some(&front))
        .expect("a concrete target has a probe");
    let drive = std::sync::Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel: channel.get(),
        handoff: std::sync::Arc::new(|_| {
            Box::pin(async { panic!("a dead pane must not wake the drive") })
        }),
        settled: None,
    });
    held_watch::arm(channel.get(), probe);
    let (_, observed) = held_watch::watch_observed(drive, 1);
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.verdict, held_watch::Verdict::End);
    assert_eq!(mailbox.has_active_turn().await, Ok(true));
    assert!(
        mailbox
            .v2_request(|s| Ok(s.as_ref().unwrap().front().is_some()))
            .await
            .unwrap()
    );
}
