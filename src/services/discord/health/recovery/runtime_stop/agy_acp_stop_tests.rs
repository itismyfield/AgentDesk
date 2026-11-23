//! The channel stop services reach a running ACP turn: a preserving stop asks agy to cancel and
//! keeps its child, a force-kill ends only that destination's child tree, also mid-cancel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::stop_judgement::judged_finish_tests::{Channel, claude};
use crate::services::agy_acp::lifecycle_tests::{acp_turn, alive, eventually, log, pid_file};
use crate::services::provider::CancelToken;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};
use crate::test_support::wait::{WAIT_CAP, await_within, join_within};

/// Under the 15 s cancel deadline: only a forced stop ends a silent child this fast.
const FORCED: Duration = Duration::from_secs(10);

#[test]
fn a_force_kill_ends_the_acp_child_at_once_and_a_preserving_stop_keeps_it() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut runtime = tokio::runtime::Builder::new_current_thread();
    let runtime = runtime.enable_all().build().unwrap();
    let other = tempfile::tempdir().unwrap();
    let other_token = Arc::new(CancelToken::new());
    let other_turn = acp_turn(8_339_200, other.path(), "group", other_token.clone());
    let tree = |dir: &std::path::Path| {
        let pid = |name| pid_file(dir, &format!("group.log.{name}"));
        (pid("pid"), pid("grandchild"))
    };
    let kept = tree(other.path());
    // (preserving stop, force-kill) per row; "cancel" answers session/cancel, "group" never does.
    for (id, mode, preserve, force) in [
        (8_339_210, "cancel", true, false),
        (8_339_220, "group", false, true),
        (8_339_230, "group", true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let (turn, started) = runtime.block_on(async {
            let fx = Channel::new(id).await;
            let turn = acp_turn(id, dir, mode, fx.judged.clone());
            assert!(eventually(WAIT_CAP, || log(dir, mode).contains("session/prompt")));
            let started = Instant::now();
            let target = TurnLifecycleTarget {
                provider: Some(claude()),
                channel_id: Some(fx.channel),
                tmux_name: String::new(),
            };
            let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
            let preserving = async {
                if preserve {
                    stop(Some(&fx.registry), &target, "acp preserve").await;
                }
            };
            let killing = async {
                let asked = async {
                    while preserve && !log(dir, mode).contains("session/cancel") {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                };
                await_within(asked, WAIT_CAP).await;
                if force {
                    let kill = force_kill_turn(Some(&fx.registry), &target, "acp force", "acp");
                    assert!(!kill.await.host_guard_kept(), "{mode}: the force-kill ran");
                }
            };
            tokio::join!(preserving, killing);
            (turn, started)
        });
        // §4.1(a): a token-cancelled turn adds no terminal message; turn_bridge settles it.
        assert_eq!(join_within(turn, WAIT_CAP), (Ok(()), vec![]), "{id}");
        // A force-kill alone may also send session/cancel before it reads the forced stop.
        assert!(
            !preserve || log(dir, mode).contains("session/cancel"),
            "{id}"
        );
        if force {
            assert!(started.elapsed() < FORCED, "{id}: no cancel deadline wait");
            let (child, grandchild) = tree(dir);
            assert!(eventually(WAIT_CAP, || !alive(child) && !alive(grandchild)));
        } else {
            let child = pid_file(dir, &format!("{mode}.log.pid"));
            assert!(alive(child), "a preserving stop keeps the resident child");
            crate::services::process::kill_pid_tree(child);
        }
    }
    assert!(alive(kept.0) && alive(kept.1) && alive(std::process::id()));
    other_token.publish_cancel("session_teardown");
    join_within(other_turn, WAIT_CAP).0.unwrap();
    assert!(eventually(WAIT_CAP, || !alive(kept.0) && !alive(kept.1)));
}
