//! The Codex discovery pass the idle relay step runs off the async runtime, and the one relay's
//! target across runtimes.

use super::super::codex_idle_rollout::codex_discovery_pass;
use super::*;
use crate::services::tui_prompt_dedupe as dedupe;
use std::time::Duration;

// A panicked pass is logged by the step, never propagated into the relay loop.
#[test]
fn a_panicked_discovery_pass_does_not_end_the_step() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(codex_discovery_pass(|| panic!("discovery pass panicked")));
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
    /// still from the one relay task.
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

    // T5/T6: a runtime replaced in process moves every relay use to the new runtime.
    #[test]
    fn a_replaced_runtime_takes_over_every_relay_use() {
        relay_fixture(async || {
            let (a, b) = (make_shared(), make_shared());
            a_then_b(&a, &b).await;
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
