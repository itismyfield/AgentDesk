//! The O actor's unsettled-rotation projection across clears, and the host's readiness view of it.

use super::*;
use crate::services::tui_o::writer::actor::run_projecting;

fn clear_hop(seq: u64, old: &SourceId, new: &SourceId) -> BindingEvent {
    let target = BindingTarget::Source(new.clone());
    bound(seq, Some(old), target, BindingCause::Clear, Some(old))
}

fn beside(path: &Path, name: &str, session: &str, body: &[u8]) -> (PathBuf, SourceId) {
    let next = path.with_file_name(name);
    std::fs::write(&next, body).unwrap();
    let source = source_id_for(session, &next).unwrap();
    (next, source)
}

fn unsettled(projection: &watch::Receiver<Option<usize>>) -> Option<usize> {
    *projection.borrow()
}

// T-C5/T-C7: after a clear the old tail posts before the new source and nothing is posted twice;
// the old source counts as unsettled until O retires it, after its successor was bound.
#[tokio::test(start_paused = true)]
async fn a_cleared_source_stays_unsettled_until_retired_and_its_tail_posts_once() {
    let (harness, a_path, a) = switched_over(&row("m0", "before the clear"));
    let bindings = Arc::new(FakeBindings::new());
    let startup = BindingTarget::Source(a.clone());
    bindings.commit(bound(1, None, startup, BindingCause::Startup, None));
    harness.gate.acquired();
    let (projected, projection) = watch::channel(None);
    let (stop, stopped) = watch::channel(false);
    let resumed = watch::channel(false).0;
    let writer = harness.writer();
    let actor = run_projecting(
        writer,
        ShadowProvider::Claude,
        bindings.clone(),
        stopped,
        resumed,
        projected,
    );
    let task = tokio::spawn(actor);
    append(&a_path, &row("m1", "first"));
    polls(3).await;
    assert_eq!(unsettled(&projection), Some(0));
    append(&a_path, &row("m2", "old tail"));
    let (_, b) = beside(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
    bindings.commit(clear_hop(2, &a, &b));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "old tail", "new first"]);
    assert_eq!(unsettled(&projection), Some(1), "bound but not retired");
    polls(12).await;
    assert_eq!(unsettled(&projection), Some(0), "retired once quiet");
    assert_eq!(harness.port.posts(), ["first", "old tail", "new first"]);
    halt(stop, task).await;
}

// T-C6: three clears with a prompt each inside ten seconds: every prompt posts once, and each
// clear leaves the rotation unsettled until O retires the sources it left.
#[tokio::test(start_paused = true)]
async fn clears_repeated_within_ten_seconds_stay_unsettled_until_all_retire() {
    let (harness, a_path, a) = switched_over(&row("m0", "before the clears"));
    let bindings = Arc::new(FakeBindings::new());
    let startup = BindingTarget::Source(a.clone());
    bindings.commit(bound(1, None, startup, BindingCause::Startup, None));
    harness.gate.acquired();
    let (projected, projection) = watch::channel(None);
    let (stop, stopped) = watch::channel(false);
    let resumed = watch::channel(false).0;
    let writer = harness.writer();
    let actor = run_projecting(
        writer,
        ShadowProvider::Claude,
        bindings.clone(),
        stopped,
        resumed,
        projected,
    );
    let task = tokio::spawn(actor);
    polls(2).await;
    let mut old = a;
    let mut posted = Vec::new();
    for (n, name) in ["b", "c", "d"].into_iter().enumerate() {
        let text = format!("prompt {name}");
        let (_, next) = beside(&a_path, &format!("{name}.jsonl"), name, &row(name, &text));
        bindings.commit(clear_hop(n as u64 + 2, &old, &next));
        polls(3).await;
        posted.push(text);
        assert_eq!(
            harness.port.posts(),
            posted,
            "{name}: each prompt posts once"
        );
        assert!(
            unsettled(&projection).is_some_and(|count| count >= 1),
            "{name}: a clear inside the quiet window is unsettled"
        );
        old = next;
    }
    polls(15).await;
    assert_eq!(unsettled(&projection), Some(0), "all retired");
    assert_eq!(harness.port.posts(), posted);
    halt(stop, task).await;
}

// T-C9: an unreadable rotation record is no answer, and an ended actor reports none.
#[tokio::test(start_paused = true)]
async fn an_unreadable_rotation_or_an_ended_actor_reports_no_projection() {
    use crate::services::tui_o::store::rotation::BOUNDARY_FILE;
    let (harness, a_path, a) = switched_over(&row("m0", "before"));
    let bindings = Arc::new(FakeBindings::new());
    let startup = BindingTarget::Source(a.clone());
    bindings.commit(bound(1, None, startup, BindingCause::Startup, None));
    harness.gate.acquired();
    let (projected, projection) = watch::channel(None);
    let (stop, stopped) = watch::channel(false);
    let resumed = watch::channel(false).0;
    let writer = harness.writer();
    let actor = run_projecting(
        writer,
        ShadowProvider::Claude,
        bindings.clone(),
        stopped,
        resumed,
        projected,
    );
    let task = tokio::spawn(actor);
    polls(2).await;
    assert_eq!(unsettled(&projection), Some(0));
    let store = a_path.parent().unwrap().join("o_store");
    let boundary = store.join(CHANNEL.to_string()).join(BOUNDARY_FILE);
    std::fs::write(&boundary, b"{not json").unwrap();
    polls(2).await;
    assert_eq!(unsettled(&projection), None, "unreadable is not zero");
    halt(stop, task).await;
    assert!(
        projection.has_changed().is_err(),
        "the ended actor dropped it"
    );
}

// The host keeps each hosted actor's projection beside its readiness; a channel without a
// running actor has none.
#[tokio::test(start_paused = true)]
async fn the_readiness_map_reports_a_running_actors_projection_only() {
    let (harness, _, source) = switched_over(&row("m0", "before the switch"));
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    harness.gate.acquired();
    assert_eq!(ready.rotation_unsettled(CHANNEL), None, "no actor yet");
    let hosts = hosted(&harness, &io, true, &ready);
    polls(3).await;
    assert_eq!(ready.rotation_unsettled(CHANNEL), Some(0));
    assert_eq!(ready.rotation_unsettled(OTHER), None);
    hosts.iter().for_each(|host| host.abort());
    polls(3).await;
    assert_eq!(ready.rotation_unsettled(CHANNEL), None, "an ended actor");
}
