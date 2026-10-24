//! Each hosted actor run answers transition cleanup requests on its own port only, and a run no
//! request reaches does none of the cleanup's work.

use super::*;
use crate::services::tui_o::store::fault::{self, Keep, Step};
use crate::services::tui_o::writer::actor::transition_hooks as hooks;
use crate::services::tui_o::writer::transition_cleanup::Request;
use crate::services::tui_o::writer::transition_port::{Context, Operation, Refused};

fn inspect() -> Operation {
    Operation::Inspect(Request {
        channel: CHANNEL,
        cutoff: 1,
        job_id: "hrot-host".into(),
        operator: "operator".into(),
        at: Utc::now(),
        reason: "herdr transition".into(),
        termination: None,
    })
}

fn context() -> Context {
    Context {
        codex: None,
        tmux: "tmux".into(),
        nonce: "nonce".into(),
        lock: Arc::default(),
    }
}

/// A hosted Claude channel switched over a transcript, its readiness and the transcript's path.
fn hosted_claude() -> (
    Harness,
    Arc<TestIo>,
    Arc<Readiness>,
    Vec<tokio::task::JoinHandle<()>>,
    PathBuf,
) {
    let (harness, path, source) = switched_over(&row("m0", "before the switch"));
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    harness.gate.acquired();
    let tasks = hosted(&harness, &io, true, &ready);
    (harness, io, ready, tasks, path)
}

// A restart of the same claim takes a new port: the old mailbox is closed, a request naming the old
// run is refused, and the old run's reply is no longer consumed as current.
#[tokio::test(start_paused = true)]
async fn transition_port_of_a_restarted_run_refuses_the_old_runs_requests_and_replies() {
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (harness, io, ready, _tasks, path) = hosted_claude();
    polls(3).await;
    let a = ready
        .transition(CHANNEL)
        .expect("the hosted run registered its port");
    let delayed = a.request(inspect(), context()).await.unwrap();
    assert_eq!(delayed.refused, None, "[T3:answered]");
    let failed = fault::plant(
        &root(&harness),
        Step::Append(Keep::Nothing),
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    append(&path, &row("m1", "after"));
    let mut b = None;
    for _ in 0..60 {
        polls(1).await;
        if !a.is_open()
            && let Some(port) = ready.transition(CHANNEL)
        {
            b = Some(port);
            break;
        }
    }
    drop(failed);
    let b = b.unwrap_or_else(|| panic!("restart: {:?}", io.alarms.0.lock().unwrap()));
    let consumed = std::cell::Cell::new(0);
    let stale = ready.with_transition_reply(CHANNEL, &a, delayed, |_| consumed.set(1));
    assert_eq!(stale, Err("stale_actor_reply"), "[T3:old_reply]");
    assert_eq!(consumed.get(), 0, "[T3:old_reply]");
    let ended = a.request(inspect(), context()).await.err();
    assert_eq!(ended.as_deref(), Some("actor ended"), "[T3:old_mailbox]");
    let foreign = b.request_as(&a, inspect(), context()).await.unwrap();
    assert_eq!(
        foreign.refused,
        Some(Refused::ForeignRun),
        "[T3:named_old_run]"
    );
    let current = b.request(inspect(), context()).await.unwrap();
    let job = ready.with_transition_reply(CHANNEL, &b, current, |reply| reply.job_id.clone());
    assert_eq!(job.as_deref(), Ok("hrot-host"), "[T3:current_reply]");
}

// A hosted actor whose port no request reaches posts and alarms as before, builds no cleanup
// snapshot, starts no probe and writes no cleanup field.
#[tokio::test(start_paused = true)]
async fn transition_port_without_requests_leaves_the_hosted_actor_as_it_was() {
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (snapshots, workers) = (hooks::snapshots(), hooks::started());
    let (harness, io, ready, _tasks, path) = hosted_claude();
    polls(3).await;
    assert!(ready.transition(CHANNEL).is_some(), "[T17:registered]");
    append(&path, &row("m1", "first"));
    polls(3).await;
    append(&path, &row("m2", "second"));
    polls(30).await;
    assert_eq!(harness.port.posts(), ["first", "second"], "[T17:posts]");
    assert_eq!(*io.alarms.0.lock().unwrap(), [], "[T17:alarms]");
    let read = (hooks::snapshots() - snapshots, hooks::started() - workers);
    assert_eq!(read, (0, 0), "[T17:no_cleanup_reads]");
    let cursors = root(&harness)
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("cursor");
    for entry in std::fs::read_dir(cursors).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("transition_cleanup"),
            "[T17:cursor_bytes] {text}"
        );
    }
}
