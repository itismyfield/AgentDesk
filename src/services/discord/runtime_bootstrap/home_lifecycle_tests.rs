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
