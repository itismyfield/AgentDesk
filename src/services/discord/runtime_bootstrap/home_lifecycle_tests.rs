//! Ordinary gateway writers retain their detached lifetime across runtime shutdown boundaries.
use super::*;
use crate::services::tui_o::cutover;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::writer::host::{self, HostParts, ManagedWriterHandle, Readiness};
use std::time::Duration;
/// Writers started for `channels` under a held gate, each in this root; a channel without a
/// source panics its host at start, so its end is never confirmed.
pub(in crate::services::discord::runtime_bootstrap) struct Hosted {
    pub(in crate::services::discord::runtime_bootstrap) _root: tempfile::TempDir,
    pub(in crate::services::discord::runtime_bootstrap) ready: Arc<Readiness>,
    pub(in crate::services::discord::runtime_bootstrap) handles: Vec<ManagedWriterHandle>,
}

/// Needs the channels forced as candidates on this thread.
pub(in crate::services::discord::runtime_bootstrap) fn hosted(
    channels: &[u64],
    sourceless: &[u64],
) -> Hosted {
    let root = tempfile::tempdir().unwrap();
    let source = |channel: &u64| {
        let transcript = root.path().join(format!("{channel}.jsonl"));
        std::fs::write(&transcript, b"").unwrap();
        (
            *channel,
            source_id_for(&format!("s{channel}"), &transcript).unwrap(),
        )
    };
    let io = host::test_io::TestHost::new(channels.iter().map(source).collect::<Vec<_>>());
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let ready: Arc<Readiness> = Arc::default();
    let (runtime_root, readiness) = (root.path().to_path_buf(), Arc::clone(&ready));
    let held = Arc::clone(&gate);
    let parts = move || HostParts {
        io,
        runtime_root: Some(runtime_root),
        gate: held,
        readiness,
    };
    let all = |owned: &(u64, _, _)| channels.contains(&owned.0) || sourceless.contains(&owned.0);
    let owned = cutover::boot_ownership().into_iter().filter(all).collect();
    let handles = host::start_managed(ShadowProvider::Claude, true, owned, parts);
    Hosted {
        _root: root,
        ready,
        handles,
    }
}

pub(in crate::services::discord::runtime_bootstrap) async fn eventually(
    seconds: u64,
    mut done: impl FnMut() -> bool,
) -> bool {
    for _ in 0..seconds * 50 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

/// One SIGTERM handler per runtime that keeps its shutdown slot, none where the slot is returned.
#[test]
fn every_slot_holding_runtime_registers_one_sigterm_handler() {
    const BOOT: &str = include_str!("../runtime_bootstrap.rs");
    let body = BOOT.split_once("pub(crate) async fn run_bot(").unwrap().1;
    let body = body.split_once("\n}\n").unwrap().0;
    let handler = "run_bot_spawn_sigterm_handler(&shared, provider.clone());";
    let branch = |from: &str, to: &str| {
        let rest = body.split_once(from).expect(from).1;
        rest.split_once(to).expect(to).0.matches(handler).count()
    };
    assert_eq!(body.matches(handler).count(), 3);
    assert_eq!(branch("if !modules.gateway {", "return;"), 1, "REST worker");
    assert_eq!(
        branch("GatewayLeaseOutcome::Standby => {", "return;"),
        1,
        "standby"
    );
    // The lease match's last arm closes it; everything after belongs to the gateway branch.
    let gateway = "GatewayLeaseOutcome::Failed => {";
    assert_eq!(
        branch(gateway, "run_bot_start_gateway_runtime("),
        1,
        "gateway"
    );
    let utility = "should_skip_agent_runtime_launch(token) {";
    assert_eq!(branch(utility, "return;"), 0, "utility");
    assert_eq!(
        branch("!gateway_outcome.starts_provider_runtime() {", "return;"),
        0,
        "lease failure"
    );
    assert!(!include_str!("gateway_runtime.rs").contains("run_bot_spawn_sigterm_handler"));
}

// The writer recovers through an enabled store, which Windows refuses without a directory fsync.
#[cfg(unix)]
#[test]
fn ordinary_gateway_writer_starts_detached_and_backend_exit_does_not_join_it() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_gateway_writer_starts_detached_and_backend_exit_does_not_join_it"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    const CHANNEL: u64 = 9_300_000_000_000_201;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _channels = cutover::test_override::force_candidates(&[(
            CHANNEL,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        )]);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let readiness = host::process_readiness();
        assert!(!readiness.is_hosted(CHANNEL));
        super::super::o_writer_host::spawn(&shared, &provider, None, true);
        assert!(
            readiness.is_hosted(CHANNEL),
            "ordinary gateway spawn starts its writer"
        );
        let backend = tokio::spawn(async { Ok::<(), &str>(()) });
        tokio::time::timeout(
            Duration::from_secs(5),
            super::finish_gateway_backend(backend, &provider, None, None, None, async {}),
        )
        .await
        .expect("backend exit does not wait on a writer registry");
        assert!(
            readiness.is_hosted(CHANNEL),
            "backend exit retains the existing detached lifetime"
        );
    });
}
