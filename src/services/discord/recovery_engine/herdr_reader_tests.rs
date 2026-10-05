//! The socket reader over a real transport and the pane gate, against an in-process Herdr server,
//! and the restart pass's endpoint switch.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLocation, HostedOwner, ProcessStamp, SourceRef,
};
use crate::services::session_host::herdr_socket_rig_tests::{
    HerdrRig, KEY, NODE, PANE, PROVIDER, SESSION, SHELL,
};

const NONCE: &str = "0123456789abcdef0123456789abcdef";

fn record(rig: &HerdrRig) -> HostedExecution {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: "AgentDesk-claude-p9c-reader".into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: NONCE.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(NONCE)),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: "claude".into(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: NONCE.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    }
}

fn stamp(pid: u32, seconds: u64) -> ProcessStamp {
    ProcessStamp {
        pid,
        start: format!("darwin:{seconds}.000000"),
    }
}

fn evidence(nonce: bool, root: ProcessStamp, provider: Option<ProcessStamp>) -> HerdrPaneReading {
    HerdrPaneReading::Present(HerdrPaneEvidence {
        binding_nonce: nonce.then(|| NONCE.to_string()),
        root: Some(root),
        provider_process: provider,
        agent_session_id: None,
    })
}

// One server's answers only: Missing needs a complete snapshot, a provider counts only when its
// environment names this execution, and two servers' replies are never combined.
#[test]
fn the_socket_reader_reports_one_servers_reading_of_the_stored_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let context = rig.context(NONCE);
    rig.run_provider(&context, false);
    let stored = record(&rig);
    let read = || SocketHerdrReader::of(&stored).unwrap().read_pane(PANE);
    let unreadable = |reading: HerdrPaneReading| matches!(reading, HerdrPaneReading::Unreadable(_));

    assert!(
        unreadable(read()),
        "an off-contract snapshot is not Missing"
    );
    rig.show_panes(&["w1-9"]);
    assert_eq!(read(), HerdrPaneReading::Missing);

    rig.show_panes(&[PANE]);
    let running = evidence(true, stamp(SHELL, 1_001), Some(stamp(PROVIDER, 1_002)));
    assert_eq!(read(), running);
    rig.foreground(&[SHELL]);
    assert_eq!(read(), evidence(false, stamp(SHELL, 1_001), None));

    // A shell started again is reported as it is now, never as the recorded one.
    rig.restart_shell(&context);
    assert_eq!(read(), evidence(false, stamp(SHELL, 1_009), None));

    rig.run_provider(&context, false);
    rig.foreground(&[PROVIDER]);
    rig.serve_as(&[7, 8, 7]);
    assert!(unreadable(read()), "a snapshot from another server");
    rig.serve_as(&[7, 7, 8]);
    assert!(unreadable(read()), "processes from another server");
    rig.serve_as(&[7]);
    assert_eq!(read(), running);

    let others = rig.requests().into_iter().filter(|request| {
        let method = request["method"].as_str().unwrap_or("");
        !matches!(method, "session.snapshot" | "pane.process_info")
    });
    assert_eq!(others.count(), 0, "only snapshots and process reads");
}

// No local endpoint: the restart pass stops at its switch, before any row, pane or hold read. With
// one it counts the held inputs that health then shows.
#[test]
fn the_restart_pass_reads_nothing_without_a_local_endpoint() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let holds = crate::config::runtime_root()
        .unwrap()
        .join("runtime/herdr_input_holds");
    std::fs::create_dir_all(&holds).unwrap();
    std::fs::write(holds.join(NONCE), "2026-10-05T07:00:00+00:00").unwrap();
    let passes = || PASSES.with(std::cell::Cell::get);
    reconnect_restarted_herdr_panes(None);
    assert_eq!(passes(), 0);
    assert_eq!(reconnect_counts(), ReconnectCounts::default());
    assert_eq!(local_reconnect_health(), None);

    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let counted = |held| Some((ReconnectCounts::default(), held));
    assert_eq!(local_reconnect_health(), counted(None));
    reconnect_restarted_herdr_panes(None);
    assert_eq!(passes(), 1, "a local endpoint gets past the switch");
    assert_eq!(local_reconnect_health(), counted(Some(Ok(1))));
}
