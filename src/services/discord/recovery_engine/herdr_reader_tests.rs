//! The socket reader over a real transport and the pane gate, against an in-process Herdr server,
//! and the restart pass's endpoint switch.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::ProcessStamp;
use crate::services::discord::recovery_engine::recovery_status_panel::footer_retirement_tests::missing_herdr_panes::{NONCE, record};
use crate::services::session_host::SocketHerdrReader;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, NODE, PANE, PROVIDER, SHELL};
use crate::services::session_host::{HerdrExecutionReader, HerdrPaneEvidence, HerdrPaneReading};

#[path = "herdr_reader_missing_tests.rs"]
mod missing;

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
    reconnect_restarted_herdr_panes(None, &crate::services::provider::ProviderKind::Claude);
    assert_eq!(passes(), 0);
    assert_eq!(reconnect_counts(), ReconnectCounts::default());
    assert_eq!(local_reconnect_health(), None);

    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let counted = |held| Some((ReconnectCounts::default(), held));
    assert_eq!(local_reconnect_health(), counted(None));
    reconnect_restarted_herdr_panes(None, &crate::services::provider::ProviderKind::Claude);
    assert_eq!(passes(), 1, "a local endpoint gets past the switch");
    assert_eq!(local_reconnect_health(), counted(Some(Ok(1))));
}
