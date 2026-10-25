//! Which gate left a restarted row: each held pass names its own reason, and a pass that finds the
//! row's end task still running names that task instead of starting another.
use super::*;

fn reason(run: &Run) -> Option<&'static str> {
    diagnostics::last_reason(&run.provider, run.channel.get())
}

const GATES: u64 = 1_479_671_399_000_131;

/// A busy composer, an open turn behind a ready one, a replaced source and a held transition each
/// keep the row under their own reason.
#[test]
fn each_held_pass_names_its_gate_pg() {
    let path = concat!(module_path!(), "::each_held_pass_names_its_gate_pg");
    in_child(
        path,
        GATES,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: GATES,
                    routed: true,
                },
            )
            .await;
            let saved = run.row.clone();
            run.relay.set(|pane| pane.screen = Some(BUSY.into()));
            run.pass().await;
            assert_eq!(reason(&run), Some("composer_busy"), "busy composer");
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            run.pass().await;
            assert_eq!(reason(&run), Some("transcript_open"), "open turn");
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            replace_source(&run.source);
            run.pass().await;
            assert_eq!(reason(&run), Some("file_identity"), "replaced source");
            let transition = run.shared.session_transition_lock(run.channel);
            let held = transition.try_lock().unwrap();
            run.pass().await;
            assert_eq!(reason(&run), Some("transition_busy"), "held transition");
            drop(held);
            assert_kept(&run, &saved, "every gate").await;
            run.close().await;
        },
    );
}

const RUNNING: u64 = 1_479_671_399_000_132;

/// A pass while the row's end task waits before its clear starts no second task and names the
/// running one.
#[test]
fn a_pass_during_a_running_end_names_the_task_pg() {
    let path = concat!(
        module_path!(),
        "::a_pass_during_a_running_end_names_the_task_pg"
    );
    in_child(
        path,
        RUNNING,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: RUNNING,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            let (reached, release) = (
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tokio::sync::Notify::new()),
            );
            let (at, go) = (reached.clone(), release.clone());
            let _hooked = super::super::on_while("before_clear", move || {
                let (at, go) = (at.clone(), go.clone());
                Box::pin(async move {
                    at.notify_one();
                    go.notified().await;
                })
            });
            row_end_pass(&run.shared, &run.provider).await;
            reached.notified().await;
            row_end_pass(&run.shared, &run.provider).await;
            assert_eq!(reason(&run), Some("ending_busy"));
            release.notify_one();
            settled_tasks().await;
            assert!(run.durable().is_none(), "the running end still finished");
            run.close().await;
        },
    );
}
