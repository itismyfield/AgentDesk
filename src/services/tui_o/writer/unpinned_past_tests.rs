//! #6737 S4: a past source bound before O started is held when bound again unless it was pinned.

use super::*;

/// A channel adopted over `t.jsonl` after it rotated from `p.jsonl`, which holds two old units
/// and is pinned at its end only when `pinned`.
fn adopted(pinned: bool) -> (Harness, [(PathBuf, SourceId); 2], Arc<FakeBindings>) {
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
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(p.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    let target = BindingTarget::Source(a.clone());
    bindings.commit(bound(2, Some(&p), target, BindingCause::Clear, Some(&p)));
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
