//! The Codex discovery pass as adoption's boot evidence: what it reports, and when the idle relay
//! step records it.

use super::super::codex_idle_rollout::codex_discovery_pass;
use super::*;
use crate::services::discord::codex_adoption_runtime::boot::{
    BootReadError, BootRole, DiscoveryPass, LiveSource, record_role,
};
use crate::services::discord::codex_adoption_runtime::{
    AdoptionRuntime, ObservationRuntimeId, install, installed,
};
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
) -> impl FnOnce(Option<ObservationRuntimeId>) -> DiscoveryPass + Send + 'static {
    let shared = shared.clone();
    move |observer| {
        let _view = View::pin(sessions);
        rehydrate_existing_codex_tui_bindings(&shared, observer)
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
        let reported = sorted(pass_over(shared, Some(&[A, B]))(Some(
            ObservationRuntimeId::for_tests(1),
        )));
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
        let reported = sorted(pass_over(shared, Some(&[A, B]))(Some(
            ObservationRuntimeId::for_tests(1),
        )));
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
        assert!(pass_over(shared, None)(Some(ObservationRuntimeId::for_tests(1))).is_err());
        assert_eq!(
            pass_over(shared, Some(&[]))(Some(ObservationRuntimeId::for_tests(1))),
            Ok(Vec::new())
        );
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
        let off = sorted(pass_over(shared, Some(sessions))(None));
        assert_eq!(
            off,
            [source(A, CA, false)],
            "no judged binding, no liveness probe"
        );
        let on = sorted(pass_over(shared, Some(sessions))(Some(
            ObservationRuntimeId::for_tests(1),
        )));
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
            codex_discovery_pass(shared, move |observer| {
                seen.send(observer).unwrap();
                Ok(Vec::new())
            })
            .await;
            observed.recv().unwrap()
        });
        assert_eq!(observed, None);
        assert!(installed(shared).is_none());
    });
}

mod relay_target {
    use super::super::super::codex_idle_rollout::{
        RELAY_SPAWNS, RELAY_TARGETS, RelayUse, spawn_codex_idle_rollout_relay,
    };
    use super::*;
    use crate::services::discord::host_defer_gate::tests::ScriptedTmux;

    const RELAYED: &str = "AgentDesk-codex-u32a20-relayed";

    fn ptr(shared: &Arc<SharedData>) -> usize {
        Arc::as_ptr(shared) as usize
    }

    /// The process-global relay reset, under the dedupe lock that every relay test holds.
    fn fresh_relay() {
        super::super::super::CODEX_IDLE_ROLLOUT_RELAY_STARTED.store(false, Ordering::Release);
        RELAY_SPAWNS.store(0, Ordering::Release);
        RELAY_TARGETS.lock().unwrap().clear();
    }

    /// A Codex binding the relay polls; re-registered since a discovery pass may evict it.
    fn keep_binding() {
        let binding = dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/nonexistent/u32a20.jsonl".to_owned(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        dedupe::register_tmux_runtime_binding(RELAYED, binding);
    }

    /// Waits until both relay uses have served `shared`; false after `limit`.
    async fn served(shared: &Arc<SharedData>, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while tokio::time::Instant::now() < deadline {
            keep_binding();
            let uses = RELAY_TARGETS.lock().unwrap().clone();
            let by = |used| uses.contains(&(used, ptr(shared)));
            if by(RelayUse::Discovery) && by(RelayUse::Owner) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn adopting(shared: &Arc<SharedData>) -> Arc<AdoptionRuntime> {
        let on = TuiOConfig {
            codex_history_adoption: true,
            ..TuiOConfig::default()
        };
        let runtime = install(shared, &ProviderKind::Codex, "bot", Some(&on)).unwrap();
        runtime.record_recovery_for_tests();
        record_role(Some(&runtime), BootRole::Gateway);
        runtime
    }

    /// Runs `test` with a fake tmux listing nothing, the relay reset and the dedupe lock held.
    fn relay_fixture(test: impl AsyncFnOnce()) {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let tmux = ScriptedTmux::install();
        tmux.list(&[]);
        let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        dedupe::reset_state_for_tests();
        fresh_relay();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(test());
        drop(rt);
        fresh_relay();
        dedupe::reset_state_for_tests();
    }

    /// Starts the relay for A, then for B: after B's first served cycle every relay use serves B,
    /// still from the one relay task. Off, this is the existing relay's routing.
    async fn a_then_b(a: &Arc<SharedData>, b: &Arc<SharedData>) {
        spawn_codex_idle_rollout_relay(a.clone());
        assert!(served(a, Duration::from_secs(10)).await, "A never served");
        spawn_codex_idle_rollout_relay(b.clone());
        assert!(served(b, Duration::from_secs(15)).await, "B never served");
        let uses = RELAY_TARGETS.lock().unwrap().clone();
        let switched = uses.iter().position(|(_, at)| *at == ptr(b)).unwrap();
        let stale: Vec<_> = uses[switched..]
            .iter()
            .filter(|(_, at)| *at != ptr(b))
            .collect();
        assert!(
            stale.is_empty(),
            "a use served A after B took over: {stale:?}"
        );
        assert_eq!(RELAY_SPAWNS.load(Ordering::Acquire), 1, "one relay task");
    }

    // T5/T6 with adoption off: a runtime replaced in process moves every relay use to the new
    // runtime; this changes relay routing whatever the adoption flag says.
    #[test]
    fn a_replaced_runtime_takes_over_every_relay_use() {
        relay_fixture(async || {
            let (a, b) = (make_shared(), make_shared());
            a_then_b(&a, &b).await;
        });
    }

    // T5 with adoption on: B's own discovery passes are recorded into B, numbered from 1.
    #[test]
    fn a_replaced_runtime_records_its_own_discovery_passes() {
        relay_fixture(async || {
            let (a, b) = (make_shared(), make_shared());
            let runtime_a = adopting(&a);
            spawn_codex_idle_rollout_relay(a.clone());
            assert!(served(&a, Duration::from_secs(10)).await);
            let runtime_b = adopting(&b);
            spawn_codex_idle_rollout_relay(b.clone());
            let stop = || tokio::time::sleep(Duration::from_secs(15));
            let read = runtime_b.wait_boot(BTreeSet::new(), stop()).await;
            assert!(read.expect("B discovered").pass_for_tests() >= 1);
            assert!(runtime_a.wait_boot(BTreeSet::new(), stop()).await.is_ok());
        });
    }

    // T7: the relay target does not own a replaced runtime's SharedData.
    #[test]
    fn the_relay_target_does_not_keep_a_replaced_runtime_alive() {
        relay_fixture(async || {
            let (a, b) = (make_shared(), make_shared());
            a_then_b(&a, &b).await;
            let replaced = Arc::downgrade(&a);
            drop(a);
            // One more full cycle on B, so the relay holds no A from before the switch.
            RELAY_TARGETS.lock().unwrap().clear();
            assert!(served(&b, Duration::from_secs(15)).await);
            assert!(replaced.upgrade().is_none(), "something still owns A");
        });
    }

    // A relay whose only runtime ended keeps polling with no target, and the next runtime to
    // start it is served by that same task.
    #[test]
    fn a_relay_left_without_a_runtime_serves_the_next_one() {
        relay_fixture(async || {
            let a = make_shared();
            spawn_codex_idle_rollout_relay(a.clone());
            assert!(served(&a, Duration::from_secs(10)).await, "A never served");
            let ended = Arc::downgrade(&a);
            drop(a);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while ended.upgrade().is_some() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(ended.upgrade().is_none(), "A outlived its last owner");
            // Several 500ms polls pass with the target gone.
            RELAY_TARGETS.lock().unwrap().clear();
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert!(
                RELAY_TARGETS.lock().unwrap().is_empty(),
                "a use served a dead runtime"
            );
            let b = make_shared();
            spawn_codex_idle_rollout_relay(b.clone());
            assert!(served(&b, Duration::from_secs(15)).await, "B never served");
            assert_eq!(RELAY_SPAWNS.load(Ordering::Acquire), 1, "one relay task");
        });
    }

    // P2-3: a single runtime (the normal process) is served on every cycle by its one relay.
    #[test]
    fn a_single_runtime_keeps_its_one_relay() {
        relay_fixture(async || {
            let a = make_shared();
            spawn_codex_idle_rollout_relay(a.clone());
            assert!(served(&a, Duration::from_secs(10)).await);
            RELAY_TARGETS.lock().unwrap().clear();
            assert!(served(&a, Duration::from_secs(15)).await, "a later cycle");
            let uses = RELAY_TARGETS.lock().unwrap().clone();
            assert!(uses.iter().all(|(_, at)| *at == ptr(&a)));
            assert_eq!(RELAY_SPAWNS.load(Ordering::Acquire), 1);
        });
    }

    fn make_shared() -> Arc<SharedData> {
        crate::services::discord::make_shared_data_for_tests()
    }
}
