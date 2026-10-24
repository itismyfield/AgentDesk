//! A past source bound before O started is held when bound again unless it was pinned.

use super::*;
use crate::services::tui_o::store::rotation::Boundary;

/// A channel adopted over `t.jsonl` after it rotated from `p.jsonl`, which holds two old units
/// and is pinned at its end only when `pinned`.
fn adopted(pinned: bool) -> (Harness, [(PathBuf, SourceId); 2], Arc<FakeBindings>) {
    adopted_as(pinned, false)
}

/// As `adopted`, with the log naming `p.jsonl` by an empty session when `unnamed`, as a Claude
/// hook may before it fills the session in.
fn adopted_as(
    pinned: bool,
    unnamed: bool,
) -> (Harness, [(PathBuf, SourceId); 2], Arc<FakeBindings>) {
    let mut named = None;
    let harness = Harness::build_channel(CHANNEL, |runtime| {
        let mut sources = Vec::new();
        let mut paths = Vec::new();
        let old = [row("p1", "old one"), row("p2", "old two")].concat();
        let current = row("m0", "current before");
        for (name, session, body) in [("t.jsonl", "s1", &current), ("p.jsonl", "s0", &old)] {
            let path = runtime.join(name);
            std::fs::write(&path, body).unwrap();
            let source_id = source_id_for(session, &path).unwrap();
            paths.push((path, source_id.clone()));
            let (delivery_start, prefix_hash) =
                (body.len() as u64, hex::encode(Sha256::digest(body)));
            sources.push(InitSource {
                source_id,
                delivery_start,
                prefix_hash,
            });
        }
        named = Some([paths[0].clone(), paths[1].clone()]);
        sources.truncate(if pinned { 2 } else { 1 });
        sources
    });
    let [(a_path, a), (p_path, p)] = named.unwrap();
    let mut logged = p.clone();
    if unnamed {
        logged.session_id.clear();
    }
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(logged.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    let target = BindingTarget::Source(a.clone());
    bindings.commit(bound(
        2,
        Some(&logged),
        target,
        BindingCause::Clear,
        Some(&logged),
    ));
    harness.gate.acquired();
    (harness, [(a_path, a), (p_path, p)], bindings)
}

/// What the channel posts and alarms once the log binds `p.jsonl` again by `cause`.
async fn bound_again(
    pinned: bool,
    cause: BindingCause,
) -> (Vec<String>, Vec<WriterAlarm>, SourceId) {
    let (harness, [(a_path, a), (p_path, p)], bindings) = adopted(pinned);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "current after"));
    polls(3).await;
    let target = BindingTarget::Source(p.clone());
    bindings.commit(bound(3, Some(&a), target, cause, Some(&a)));
    append(&p_path, &row("p3", "past resumed"));
    polls(20).await;
    let (posts, alarms) = (harness.port.posts(), harness.alarms.taken());
    halt(stop, task).await;
    (posts, alarms, p)
}

const CAUSES: [BindingCause; 7] = [
    BindingCause::Startup,
    BindingCause::Resume,
    BindingCause::Clear,
    BindingCause::Compact,
    BindingCause::Continuation,
    BindingCause::Fork,
    BindingCause::Unknown,
];

#[tokio::test(start_paused = true)]
async fn an_unpinned_past_source_bound_again_posts_none_of_its_records_whatever_the_cause() {
    for cause in CAUSES {
        let (posts, alarms, p) = bound_again(false, cause).await;
        assert_eq!(posts, ["current after"], "{cause:?}");
        let pending = WriterAlarm::BoundaryPending { source: p };
        assert_eq!(alarms, [pending], "{cause:?} waits for an operator's start");
    }
}

#[tokio::test(start_paused = true)]
async fn a_pinned_past_source_bound_again_posts_only_what_follows_its_pinned_end() {
    for cause in CAUSES {
        let (posts, alarms, _) = bound_again(true, cause).await;
        assert_eq!(posts, ["current after", "past resumed"], "{cause:?}");
        assert_eq!(alarms, [], "{cause:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_unpinned_past_source_logged_without_its_session_is_held_when_bound_with_it() {
    for cause in [BindingCause::Startup, BindingCause::Clear] {
        let (harness, [(a_path, a), (p_path, p)], bindings) = adopted_as(false, true);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&a_path, &row("m1", "current after"));
        polls(3).await;
        let target = BindingTarget::Source(p.clone());
        bindings.commit(bound(3, Some(&a), target, cause, Some(&a)));
        append(&p_path, &row("p3", "past resumed"));
        polls(20).await;
        assert_eq!(harness.port.posts(), ["current after"], "{cause:?}");
        let pending = WriterAlarm::BoundaryPending { source: p };
        assert_eq!(harness.alarms.taken(), [pending], "{cause:?}");
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn another_file_at_an_unnamed_past_path_is_not_held() {
    let (harness, [(a_path, a), (p_path, _)], bindings) = adopted_as(false, true);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "current after"));
    polls(3).await;
    // Written beside and renamed over, so the new file cannot reuse the old one's inode.
    let next = p_path.with_extension("next");
    std::fs::write(&next, row("n1", "new file")).unwrap();
    std::fs::rename(&next, &p_path).unwrap();
    let fresh = source_id_for("s0", &p_path).unwrap();
    let target = BindingTarget::Source(fresh);
    bindings.commit(bound(3, Some(&a), target, BindingCause::Clear, Some(&a)));
    polls(20).await;
    assert_eq!(harness.port.posts(), ["current after", "new file"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_held_past_source_resolved_at_its_end_posts_only_later_records_across_restarts() {
    let (harness, [(a_path, a), (p_path, p)], bindings) = adopted(false);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "current after"));
    polls(3).await;
    let target = BindingTarget::Source(p.clone());
    bindings.commit(bound(3, Some(&a), target, BindingCause::Clear, Some(&a)));
    append(&p_path, &row("p3", "past resumed"));
    polls(20).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["current after"]);
    let pending = WriterAlarm::BoundaryPending { source: p.clone() };
    assert_eq!(harness.alarms.taken(), [pending]);
    let end = std::fs::metadata(&p_path).unwrap().len();
    let resolved = harness.store.seed_boundary_resolved(CHANNEL, &p, end);
    assert_eq!(resolved.unwrap(), (p.clone(), end));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&p_path, &row("p4", "after the resolution"));
    polls(6).await;
    halt(stop, task).await;
    assert_eq!(
        harness.port.posts(),
        ["current after", "after the resolution"]
    );
    let boundary = |h: &Harness| {
        h.channel()
            .rotation()
            .unwrap()
            .link(&p)
            .map(|l| l.boundary.clone())
    };
    assert_eq!(boundary(&harness), Some(Boundary::Owed { from: end }));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(6).await;
    halt(stop, task).await;
    assert_eq!(
        harness.port.posts(),
        ["current after", "after the resolution"],
        "posted once"
    );
    assert_eq!(boundary(&harness), Some(Boundary::Owed { from: end }));
    assert_eq!(harness.alarms.taken(), []);
}
