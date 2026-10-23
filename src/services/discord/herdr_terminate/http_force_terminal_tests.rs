//! A force-kill whose effects meet a dispatch another writer already finished keeps that terminal
//! status, retry off or on: no failure is written over it and no retry is made from it.

use super::super::Node;
use super::*;

/// The rig for test `name` in its own child, on a current-thread runtime so every probe runs.
fn rig_terminal(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    if !herdr::in_child(&format!("{}::{name}", module_path!()), CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, false);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

/// Every terminal status with retry off, then on; each row runs on its own database and runtime.
const ROWS: [(&str, &str, bool); 6] = [
    ("completed-retry-off", "completed", false),
    ("completed-retry-on", "completed", true),
    ("cancelled-retry-off", "cancelled", false),
    ("cancelled-retry-on", "cancelled", true),
    ("failed-retry-off", "failed", false),
    ("failed-retry-on", "failed", true),
];

// The effects reach the terminal dispatch and leave it as it was: same status, no retry row, and
// the retired run still disconnects the session and answers success with no retry id.
#[test]
fn e08_terminal_dispatch_is_preserved_pg() {
    let Some((rig, runtime)) = rig_terminal("e08_terminal_dispatch_is_preserved_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let mut rows_ran = 0;
        let mut node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        for (name, terminal, retry) in ROWS {
            node = Node::again_e08(node, &rig).await;
            node.start(51, "turn-a", Some(DISPATCH)).await;
            let finished = "UPDATE task_dispatches SET status = $2 WHERE id = $1";
            let query = sqlx::query(finished).bind(DISPATCH).bind(terminal);
            assert_eq!(query.execute(&node.pool).await.unwrap().rows_affected(), 1);
            let (postlude, released) = (ran(), ran());
            let flag = postlude.clone();
            probe_before_postlude(Box::new(move || {
                Box::pin(async move { flag.store(true, Ordering::SeqCst) })
            }));
            let flag = released.clone();
            probe_before_released(Box::new(move || {
                Box::pin(async move { flag.store(true, Ordering::SeqCst) })
            }));
            let (status, reply) = node.kill(retry).await;
            let after = node.killed(&rig).await;
            assert_eq!(
                after.dispatches,
                dispatches(terminal, "dispatched"),
                "{name}: {reply}"
            );
            assert!(
                after.retries.is_empty(),
                "{name}: no retry from a finished dispatch"
            );
            assert!(
                postlude.load(Ordering::SeqCst),
                "{name}: the effects were reached"
            );
            assert!(
                released.load(Ordering::SeqCst),
                "{name}: the released step ran"
            );
            let answered = (StatusCode::OK, result("retired", "retired"));
            assert_eq!(
                (status, reply["termination"].clone()),
                answered,
                "{name}: {reply}"
            );
            assert_eq!(reply["retry_dispatch_id"], Value::Null, "{name}");
            assert_eq!(reply["retry_skipped_reason"], Value::Null, "{name}");
            assert_eq!(after.closes(), 1, "{name}");
            assert_eq!(after.session, kept("disconnected", None), "{name}");
            assert_eq!((after.row, after.held), (retired(), false), "{name}");
            rows_ran += 1;
        }
        assert_eq!(rows_ran, ROWS.len(), "every row ran");
        node.finish().await;
    });
}
