use super::switch_tests::{link, spool};
use super::*;
use crate::services::tui_o::store::rotation::{Boundary, ResolveFrom, Rotation};
use crate::services::tui_o::store::spool::source_key;

fn line(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

/// A native user prompt: a row identity without any posted unit.
fn prompt(uuid: &str, text: &str) -> Vec<u8> {
    line(serde_json::json!({"type": "user", "uuid": uuid, "message": {"content": text}}))
}

/// A user row of error tool results, each posted as its own unit.
fn results(uuid: &str, results: &[(&str, &str)]) -> Vec<u8> {
    let item = |(id, text): &(&str, &str)| serde_json::json!({"type": "tool_result", "tool_use_id": id, "is_error": true, "content": text});
    let content: Vec<_> = results.iter().map(item).collect();
    line(serde_json::json!({"type": "user", "uuid": uuid,
        "timestamp": Utc::now().to_rfc3339(), "message": {"content": content}}))
}

/// An assistant row the identity rules cannot place.
fn blocked(uuid: &str) -> Vec<u8> {
    line(
        serde_json::json!({"type": "assistant", "uuid": uuid, "apiBlockIndex": 0,
        "timestamp": Utc::now().to_rfc3339(), "message": {"id": uuid,
        "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}}),
    )
}

fn fork(seq: u64, old: &SourceId, new: &SourceId) -> BindingEvent {
    rotate(seq, old, new, BindingCause::Fork)
}

fn boundary(harness: &Harness, source: &SourceId) -> Boundary {
    let rotation = harness.channel().rotation().unwrap();
    rotation.link(source).unwrap().boundary.clone()
}

/// Same bytes at the parent's path under a new inode.
fn replace(path: &Path) {
    let swap = path.with_extension("swap");
    std::fs::write(&swap, std::fs::read(path).unwrap()).unwrap();
    std::fs::rename(&swap, path).unwrap();
}

struct Crashed {
    harness: Harness,
    a_path: PathBuf,
    b_path: PathBuf,
    b: SourceId,
    bindings: Arc<FakeBindings>,
}

/// A writer stopped while `b`, forked from `a` at seq 2, was undecided with every row spooled.
fn crashed_fork(
    a_body: &[u8],
    b_body: &[u8],
    retire: impl Fn(&mut ChannelStore, &SourceId, &SourceId),
) -> Crashed {
    let (harness, a_path, a, bindings) = started(a_body);
    let runtime = a_path.parent().unwrap().to_path_buf();
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", b_body);
    bindings.commit(fork(2, &a, &b));
    let mut store = harness.channel();
    let mut rotation = Rotation::default();
    let undecided = link(&b, 2, Some(&a), Boundary::Undecided);
    rotation.links.insert(source_key(&b), undecided);
    rotation.successors.insert(source_key(&a), b.clone().into());
    store.write_rotation(&rotation).unwrap();
    store.attach_source(&b).unwrap();
    store.set_binding_checkpoint(2).unwrap();
    spool(&mut store, &runtime, &b, false);
    retire(&mut store, &a, &b);
    drop(store);
    Crashed {
        harness,
        a_path,
        b_path,
        b,
        bindings,
    }
}

#[tokio::test(start_paused = true)]
async fn a_fork_is_owed_from_its_first_new_row_whatever_its_timestamps_say() {
    let (now, stale) = (Utc::now(), Utc::now() - TimeDelta::hours(1));
    let m0 = || row_at("m0", "before the switch", now);
    let cases = [
        (
            "[N4:stale]",
            row("m0", "before the switch"),
            m0(),
            [row_at("x1", "first new", stale), row("x2", "second new")].concat(),
            vec!["first new", "second new"],
        ),
        (
            "[N4:no_ts]",
            row("m0", "before the switch"),
            m0(),
            [row("x1", "first new"), row_at("x2", "second new", now)].concat(),
            vec!["first new", "second new"],
        ),
        (
            "[N4:fresh]",
            row("m0", "before the switch"),
            m0(),
            [row_at("x1", "first new", now), row("x2", "second new")].concat(),
            vec!["first new", "second new"],
        ),
        (
            "[N4:prompt_prefix]",
            prompt("u-p1", "hello"),
            prompt("u-p1", "hello"),
            row_at("n1", "answer", now),
            vec!["answer"],
        ),
    ];
    for (tag, parent, inherited, new, expected) in cases {
        let (harness, a_path, a, bindings) = started(&parent);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let (_, b) = transcript(&a_path, "b.jsonl", "s2", &[inherited.clone(), new].concat());
        bindings.commit(fork(2, &a, &b));
        polls(3).await;
        assert_eq!(harness.port.posts(), expected, "{tag} posts");
        assert_eq!(harness.alarms.taken(), [], "{tag} alarms");
        let from = inherited.len() as u64;
        assert_eq!(
            boundary(&harness, &b),
            Boundary::Owed { from },
            "{tag} start"
        );
        halt(stop, task).await;
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(3).await;
        assert_eq!(
            harness.port.posts(),
            expected,
            "{tag} a restart reposts nothing"
        );
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_fork_that_cannot_be_matched_to_its_parent_posts_nothing_until_resolved() {
    let now = Utc::now();
    let m0 = || row("m0", "before the switch");
    let m0_copy = || row_at("m0", "before the switch", now);
    let parent_result = results("u-r1", &[("k1", "one")]);
    let cases = [
        (
            "[N4:empty]",
            m0(),
            [
                row_at("z1", "unrelated one", now),
                row_at("z2", "unrelated two", now),
            ]
            .concat(),
        ),
        (
            "[N4:mixed]",
            [m0(), parent_result].concat(),
            [m0_copy(), results("u-r2", &[("k1", "one"), ("k2", "two")])].concat(),
        ),
        (
            "[N4:unreadable]",
            m0(),
            [m0_copy(), blocked("u-blk"), row_at("n1", "after", now)].concat(),
        ),
        ("[N4:eof]", m0(), m0_copy()),
    ];
    for (tag, parent, forked) in cases {
        let (harness, a_path, a, bindings) = started(&parent);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &forked);
        bindings.commit(fork(2, &a, &b));
        polls(3).await;
        if tag == "[N4:eof]" {
            append(&b_path, &row_at("n1", "late new", now));
            polls(3).await;
        }
        let pending = WriterAlarm::BoundaryPending { source: b.clone() };
        assert!(harness.port.posts().is_empty(), "{tag} posts");
        assert_eq!(harness.alarms.taken(), [pending.clone()], "{tag} alarm");
        let held = matches!(boundary(&harness, &b), Boundary::Pending { .. });
        assert!(held, "{tag} pending");
        assert!(harness.channel().ledger().gc_segments(&b).is_empty());
        halt(stop, task).await;
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(3).await;
        assert!(
            harness.port.posts().is_empty(),
            "{tag} posts after a restart"
        );
        assert_eq!(
            harness.alarms.taken(),
            [pending],
            "{tag} alarm after a restart"
        );
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_fork_whose_parent_bytes_are_gone_posts_nothing_until_resolved() {
    let parent = [row("m0", "before the switch"), row("m1", "also before")].concat();
    let forked = [
        row_at("m0", "before the switch", Utc::now()),
        row_at("n1", "fork new", Utc::now()),
    ]
    .concat();
    for tag in ["[N4:replaced]", "[N4:short]"] {
        let retire = |store: &mut ChannelStore, a: &SourceId, _: &SourceId| {
            store.set_retired(a, true).unwrap();
        };
        let crashed = crashed_fork(&parent, &forked, retire);
        if tag == "[N4:replaced]" {
            replace(&crashed.a_path);
        } else {
            let first = row("m0", "before the switch").len() as u64;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&crashed.a_path);
            file.unwrap().set_len(first).unwrap();
        }
        let (harness, b) = (&crashed.harness, &crashed.b);
        let pending = WriterAlarm::BoundaryPending { source: b.clone() };
        for _ in 0..2 {
            let writer = harness.writer();
            let (stop, task) = spawn_with(writer, ShadowProvider::Claude, crashed.bindings.clone());
            polls(3).await;
            assert!(harness.port.posts().is_empty(), "{tag} posts");
            assert_eq!(harness.alarms.taken(), [pending.clone()], "{tag} alarm");
            let held = matches!(boundary(harness, b), Boundary::Pending { .. });
            assert!(held, "{tag} pending");
            halt(stop, task).await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_parent_row_the_channel_had_not_read_is_not_counted_as_inherited() {
    let late = row("r1", "parent late");
    let forked = [
        row_at("m0", "before the switch", Utc::now()),
        late.clone(),
        row_at("n1", "fork new", Utc::now()),
    ]
    .concat();
    let crashed = crashed_fork(&row("m0", "before the switch"), &forked, |_, _, _| {});
    append(&crashed.a_path, &late);
    let (harness, bindings) = (&crashed.harness, crashed.bindings.clone());
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(5).await;
    let posts = harness.port.posts();
    assert_eq!(posts, ["parent late", "fork new"], "[N4:consumed] posts");
    assert_eq!(harness.alarms.taken(), [], "[N4:consumed] alarms");
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_fork_decides_the_same_whether_or_not_the_writer_restarts_while_its_parent_advances() {
    for (tag, restart) in [("[N4:sync:live]", false), ("[N4:sync:restart]", true)] {
        let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
        let (mut stop, mut task) =
            spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let mixed = results("u-r", &[("k1", "one"), ("k2", "two")]);
        let (head, rest) = mixed.split_at(mixed.len() / 2);
        let forked = [row_at("m0", "before the switch", Utc::now()), head.to_vec()].concat();
        let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &forked);
        bindings.commit(fork(2, &a, &b));
        polls(3).await;
        assert_eq!(
            boundary(&harness, &b),
            Boundary::Undecided,
            "{tag} undecided"
        );
        append(&a_path, &results("u-k1", &[("k1", "one")]));
        polls(3).await;
        assert_eq!(
            harness.port.posts(),
            ["one"],
            "{tag} the parent posts its row"
        );
        if restart {
            halt(stop, task).await;
            (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
            polls(3).await;
        }
        append(&b_path, rest);
        polls(3).await;
        assert_eq!(harness.port.posts(), ["one"], "{tag} posts");
        let pending = WriterAlarm::BoundaryPending { source: b.clone() };
        assert_eq!(harness.alarms.taken(), [pending], "{tag} alarm");
        let held = matches!(boundary(&harness, &b), Boundary::Pending { .. });
        assert!(held, "{tag} pending");
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_fork_decides_the_same_whether_or_not_the_writer_restarts_after_its_retired_parent_is_replaced()
 {
    for (tag, restart) in [("[N4:recheck:live]", false), ("[N4:recheck:restart]", true)] {
        let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
        let (mut stop, mut task) =
            spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let new = row_at("n1", "fork new", Utc::now());
        let (head, rest) = new.split_at(new.len() / 2);
        let forked = [row_at("m0", "before the switch", Utc::now()), head.to_vec()].concat();
        let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &forked);
        bindings.commit(fork(2, &a, &b));
        polls(15).await;
        assert!(
            retired(&harness, &a),
            "{tag} the parent retires on the fork's evidence"
        );
        assert_eq!(
            boundary(&harness, &b),
            Boundary::Undecided,
            "{tag} undecided"
        );
        replace(&a_path);
        if restart {
            halt(stop, task).await;
            (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
            polls(3).await;
        }
        append(&b_path, rest);
        polls(3).await;
        assert!(harness.port.posts().is_empty(), "{tag} posts");
        let pending = WriterAlarm::BoundaryPending { source: b.clone() };
        assert_eq!(harness.alarms.taken(), [pending], "{tag} alarm");
        let held = matches!(boundary(&harness, &b), Boundary::Pending { .. });
        assert!(held, "{tag} pending");
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_retired_fork_left_undecided_at_its_end_is_held_for_the_operator() {
    let tag = "[N4:eof:retired-upgrade]";
    let forked = row_at("m0", "before the switch", Utc::now());
    let retire = |store: &mut ChannelStore, _: &SourceId, b: &SourceId| {
        store.set_retired(b, true).unwrap();
    };
    let crashed = crashed_fork(&row("m0", "before the switch"), &forked, retire);
    let (harness, b, b_path) = (&crashed.harness, &crashed.b, &crashed.b_path);
    let (stop, task) = spawn_with(
        harness.writer(),
        ShadowProvider::Claude,
        crashed.bindings.clone(),
    );
    polls(3).await;
    let end = forked.len() as u64;
    let held = Boundary::Pending {
        candidates: vec![0, end],
    };
    assert_eq!(boundary(harness, b), held, "{tag} pending at the end");
    let pending = WriterAlarm::BoundaryPending { source: b.clone() };
    assert_eq!(harness.alarms.taken(), [pending], "{tag} alarm");
    assert!(harness.port.posts().is_empty(), "{tag} posts");
    halt(stop, task).await;
    let path = b_path.display().to_string();
    let from = ResolveFrom::Offset(end);
    let recorded = harness
        .store
        .record_boundary_resolved(CHANNEL, &path, &from, "op");
    assert_eq!(recorded.unwrap(), (b.clone(), end), "{tag} resolvable");
    let (stop, task) = spawn_with(
        harness.writer(),
        ShadowProvider::Claude,
        crashed.bindings.clone(),
    );
    append(b_path, &row_at("n1", "after the resolution", Utc::now()));
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["after the resolution"],
        "{tag} resolved posts"
    );
    assert_eq!(boundary(harness, b), Boundary::Owed { from: end });
    let grew = WriterAlarm::RetiredSourceGrew { source: b.clone() };
    assert_eq!(harness.alarms.taken(), [grew]);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_parent_row_copied_after_the_fork_start_is_not_posted_again() {
    let parent = [row("m0", "first before"), row("m1", "second before")].concat();
    let (harness, a_path, a, bindings) = started(&parent);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    let inherited = row_at("m0", "first before", Utc::now());
    let forked = [
        inherited.clone(),
        row_at("x1", "fork new", Utc::now()),
        row_at("m1", "second before", Utc::now()),
    ]
    .concat();
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &forked);
    bindings.commit(fork(2, &a, &b));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["fork new"], "[N4:masked] posts");
    assert_eq!(harness.alarms.taken(), [], "[N4:masked] alarms");
    let from = inherited.len() as u64;
    assert_eq!(boundary(&harness, &b), Boundary::Owed { from });
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_row_a_pending_parent_also_holds_is_still_posted_after_the_fork_start() {
    let tag = "[N4:mask:pending-parent]";
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    let unrelated = || row_at("z1", "unrelated", Utc::now());
    let pending_body = [unrelated(), row("r", "shared row")].concat();
    let (_, p) = transcript(&a_path, "p.jsonl", "s2", &pending_body);
    bindings.commit(fork(2, &a, &p));
    polls(3).await;
    let held = matches!(boundary(&harness, &p), Boundary::Pending { .. });
    assert!(held, "{tag} the parent waits for the operator");
    let forked = [
        unrelated(),
        row_at("x1", "fork new", Utc::now()),
        row("r", "shared row"),
    ];
    let (_, b) = transcript(&a_path, "b.jsonl", "s3", &forked.concat());
    bindings.commit(fork(3, &p, &b));
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["fork new", "shared row"],
        "{tag} posts"
    );
    let pending = WriterAlarm::BoundaryPending { source: p };
    assert_eq!(harness.alarms.taken(), [pending], "{tag} alarms");
    halt(stop, task).await;
}
