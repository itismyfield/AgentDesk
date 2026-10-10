//! The Codex discovery pass as adoption's boot evidence: what it reports, and when the idle relay
//! step records it.

use super::super::codex_idle_rollout::codex_discovery_pass;
use super::*;
use crate::services::discord::codex_adoption_runtime::boot::{
    BootReadError, BootRole, LiveSource, record_role,
};
use crate::services::discord::codex_adoption_runtime::{AdoptionRuntime, install, installed};
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_prompt_dedupe as dedupe;
use std::collections::BTreeSet;
use std::time::Duration;

const A: &str = "AgentDesk-codex-u32a1-a";
const B: &str = "AgentDesk-codex-u32a1-b";
const CA: u64 = 6_325_201;
const CB: u64 = 6_325_202;

fn fixture(test: impl FnOnce(&Path, &Arc<SharedData>)) {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    dedupe::reset_state_for_tests();
    test(
        temp.path(),
        &crate::services::discord::make_shared_data_for_tests(),
    );
    dedupe::reset_state_for_tests();
}

/// A live Codex pane of `channel` with its rollout marker; `routed` puts it in the watcher
/// registry, the relay's routing authority.
fn live_pane(root: &Path, shared: &Arc<SharedData>, session: &str, channel: u64, routed: bool) {
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        session,
        RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    let path = root.join(format!("{session}.jsonl"));
    let meta = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\"}}}}\n");
    std::fs::write(&path, meta).unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        session,
        &path,
        Some(session),
    )
    .unwrap();
    dedupe::register_tmux_channel(session, channel);
    if routed {
        shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(session, ChannelId::new(channel));
    }
}

/// Pins this thread's tmux view, `None` an unreadable listing; cleared on unwind.
struct View;

impl View {
    fn pin(sessions: Option<&[&str]>) -> Self {
        CODEX_PASS_TMUX_UNREADABLE.set(sessions.is_none());
        let sessions = sessions
            .unwrap_or_default()
            .iter()
            .map(ToString::to_string)
            .collect();
        CODEX_PASS_TMUX_VIEW.set(Some(sessions));
        Self
    }
}

impl Drop for View {
    fn drop(&mut self) {
        CODEX_PASS_TMUX_UNREADABLE.set(false);
        CODEX_PASS_TMUX_VIEW.set(None);
    }
}

/// The production pass over `sessions`, run wherever the idle relay step runs it.
fn pass_over(
    shared: &Arc<SharedData>,
    sessions: Option<&'static [&'static str]>,
) -> impl FnOnce(bool) -> DiscoveryPass + Send + 'static {
    let shared = shared.clone();
    move |observe| {
        let _view = View::pin(sessions);
        rehydrate_existing_codex_tui_bindings(&shared, observe)
    }
}

fn sorted(pass: DiscoveryPass) -> Vec<LiveSource> {
    let mut sources = pass.expect("listing read");
    sources.sort_by(|a, b| a.key.cmp(&b.key));
    sources
}

fn source(key: &str, channel: u64, bound: bool) -> LiveSource {
    LiveSource {
        key: key.to_owned(),
        channel: Some(channel),
        bound,
    }
}

/// A Gateway runtime whose recovery is recorded, so discovery alone decides.
fn recovered(shared: &Arc<SharedData>) -> Arc<AdoptionRuntime> {
    let on = TuiOConfig {
        codex_history_adoption: true,
        ..TuiOConfig::default()
    };
    let runtime = install(shared, &ProviderKind::Codex, "bot", Some(&on)).unwrap();
    record_role(Some(&runtime), BootRole::Gateway);
    runtime.record_recovery_for_tests();
    runtime
}

fn paused() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

async fn read(runtime: &AdoptionRuntime, ids: &[u64]) -> Result<u64, BootReadError> {
    let channels: BTreeSet<u64> = ids.iter().copied().collect();
    let witness = runtime.wait_boot(channels, std::future::pending()).await?;
    Ok(witness.pass_for_tests())
}

#[test]
fn discovery_success_requires_complete_bound_inventory() {
    fixture(|root, shared| {
        live_pane(root, shared, A, CA, true);
        live_pane(root, shared, B, CB, true);
        let reported = sorted(pass_over(shared, Some(&[A, B]))(true));
        assert_eq!(reported, [source(A, CA, true), source(B, CB, true)]);
        paused().block_on(async {
            let runtime = recovered(shared);
            codex_discovery_pass(shared, pass_over(shared, Some(&[A, B]))).await;
            assert_eq!(read(&runtime, &[CA, CB]).await, Ok(1));
        });
    });
}

#[test]
fn an_unrouted_live_pane_blocks_only_its_own_channel() {
    fixture(|root, shared| {
        live_pane(root, shared, A, CA, true);
        live_pane(root, shared, B, CB, false);
        let reported = sorted(pass_over(shared, Some(&[A, B]))(true));
        assert_eq!(reported, [source(A, CA, true), source(B, CB, false)]);
        paused().block_on(async {
            let runtime = recovered(shared);
            codex_discovery_pass(shared, pass_over(shared, Some(&[A, B]))).await;
            assert_eq!(
                read(&runtime, &[CA, CB]).await,
                Err(BootReadError::TimedOut)
            );
            assert_eq!(read(&runtime, &[CA]).await, Ok(1));
        });
    });
}

#[test]
fn failed_listing_is_not_an_empty_success() {
    fixture(|_, shared| {
        assert!(pass_over(shared, None)(true).is_err());
        assert_eq!(pass_over(shared, Some(&[]))(true), Ok(Vec::new()));
        paused().block_on(async {
            let runtime = recovered(shared);
            codex_discovery_pass(shared, pass_over(shared, None)).await;
            assert_eq!(read(&runtime, &[CA]).await, Err(BootReadError::TimedOut));
            codex_discovery_pass(shared, pass_over(shared, Some(&[]))).await;
            assert_eq!(read(&runtime, &[CA]).await, Ok(2));
        });
    });
}

// The step records a pass only once it returned: a spawned, still running pass is no evidence,
// and a pass whose task failed is recorded as failed, not as an empty success.
#[test]
fn discovery_spawn_and_failed_pass_are_not_complete() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let runtime = recovered(&shared);
        let ids: BTreeSet<u64> = [CA].into();
        let soon = || tokio::time::sleep(Duration::from_millis(300));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let running = tokio::spawn({
            let shared = shared.clone();
            async move {
                codex_discovery_pass(&shared, move |_| {
                    released.recv().unwrap();
                    Ok(Vec::new())
                })
                .await;
            }
        });
        let early = runtime.wait_boot(ids.clone(), soon()).await;
        assert_eq!(early.unwrap_err(), BootReadError::Cancelled);
        release.send(()).unwrap();
        running.await.unwrap();
        assert!(runtime.wait_boot(ids.clone(), soon()).await.is_ok());

        codex_discovery_pass(&shared, |_| panic!("discovery pass panicked")).await;
        let failed = runtime.wait_boot(ids, soon()).await;
        assert_eq!(failed.unwrap_err(), BootReadError::Cancelled);
    });
}

// Off, the step finds no runtime and the pass makes none of the binding reads observation adds.
#[test]
fn off_pass_reads_no_binding_and_records_nothing() {
    fixture(|root, shared| {
        live_pane(root, shared, A, CA, true);
        let unplaced = "AgentDesk-codex-u32a1-unplaced";
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            unplaced,
            RuntimeHandoffKind::CodexTui,
        )
        .unwrap();
        let sessions: &'static [&'static str] = &[A, "AgentDesk-codex-u32a1-unplaced"];
        let off = sorted(pass_over(shared, Some(sessions))(false));
        assert_eq!(
            off,
            [source(A, CA, false)],
            "no judged binding, no liveness probe"
        );
        let on = sorted(pass_over(shared, Some(sessions))(true));
        let unplaced = LiveSource {
            key: unplaced.to_owned(),
            channel: None,
            bound: false,
        };
        assert_eq!(on, [source(A, CA, true), unplaced]);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let observed = rt.block_on(async {
            let (seen, observed) = std::sync::mpsc::channel();
            codex_discovery_pass(shared, move |observe| {
                seen.send(observe).unwrap();
                Ok(Vec::new())
            })
            .await;
            observed.recv().unwrap()
        });
        assert!(!observed);
        assert!(installed(shared).is_none());
    });
}
