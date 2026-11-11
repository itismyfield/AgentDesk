//! A reserved Codex channel taken over through the real host: a fresh rollout from its first byte,
//! one that holds output from its frontier, and Legacy sends nothing once it commits.

use super::*;
use crate::config::Config;
use std::collections::BTreeMap;

#[path = "host_codex_turn_mark_tests.rs"]
mod turn_mark;

/// The boot snapshot of `channel` reserved for late adoption and selected for turn mode.
fn reserve(channel: u64) -> test_override::ChannelsGuard {
    let pending = |_: &_, _| Ok(BTreeMap::from([(channel, Adoption::Pending)]));
    let boot = booted(channel, pending);
    assert!(boot.candidate(channel).unwrap().late_adopt_origin());
    test_override::force_boot(boot)
}

/// The boot snapshot of `channel` seeded from the store under `root` as a process start reads it.
fn rebooted(channel: u64, root: &std::path::Path) -> test_override::ChannelsGuard {
    let stored = |channels: &_, _| {
        Ok(crate::services::tui_o::channel_policy::stored(
            Some(root),
            channels,
        ))
    };
    test_override::force_boot(booted(channel, stored))
}

fn booted(
    channel: u64,
    stored: impl FnOnce(
        &std::collections::BTreeSet<u64>,
        bool,
    ) -> std::io::Result<BTreeMap<u64, Adoption>>,
) -> BootChannels {
    let config: Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": channel.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [channel], "late_adopt": true},
            "turn": {"channels": [channel]}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config).unwrap();
    boot.seeded(&config, stored).unwrap()
}

/// A first turn's opener and prompt.
fn opening() -> Vec<u8> {
    let at = Utc::now().to_rfc3339();
    [
        codex_line(serde_json::json!({"type": "event_msg", "timestamp": at,
            "payload": {"type": "task_started", "turn_id": "t1"}})),
        codex_line(serde_json::json!({"type": "response_item", "timestamp": at,
            "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]}})),
    ]
    .concat()
}

/// The prompt's own `user_message` mirror, which leaves the rollout before any output.
fn mirror() -> Vec<u8> {
    codex_line(
        serde_json::json!({"type": "event_msg", "timestamp": Utc::now().to_rfc3339(),
        "payload": {"type": "user_message", "message": "hi"}}),
    )
}

/// A fresh rollout through its first prompt, before any output.
fn prompted(session: &str) -> Vec<u8> {
    [metadata(session), opening()].concat()
}

struct Takeover {
    harness: Harness,
    io: Arc<TestIo>,
    tasks: Hosts,
    _reserved: test_override::ChannelsGuard,
}

/// A turn Codex opened and closed before the bind that resumed its rollout, by its own clock.
fn history(turn: &str, text: &str) -> Vec<u8> {
    let at = "2026-01-01T00:00:00Z";
    [
        codex_line(serde_json::json!({"type": "event_msg", "timestamp": at,
            "payload": {"type": "task_started", "turn_id": turn}})),
        codex_line(serde_json::json!({"type": "response_item", "timestamp": at,
            "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "before"}]}})),
        answer("msg_0", text),
        codex_line(serde_json::json!({"type": "event_msg", "timestamp": at,
            "payload": {"type": "task_complete", "turn_id": turn}})),
    ]
    .concat()
}

/// A host whose channel already bound a rollout whose one turn Legacy answered before the
/// baseline; a queued Legacy body holds every commit until `bind_new` binds the next rollout.
async fn hosted_over_old(channel: u64) -> Takeover {
    let harness = fresh_on(channel);
    harness.gate.acquired();
    let reserved = reserve(channel);
    p5::set_test_root(Some(harness._runtime.path()));
    let (old, _) = rollout(&harness, "rollout-old.jsonl", &metadata("s-codex"));
    bind_late(channel, &old);
    let answered = [opening(), answer("msg_0", "old answer"), turn_end()];
    append(&old, &answered.concat());
    let candidate = test_override::with_channels(|boot| boot.unwrap().candidate(channel).cloned());
    drop(candidate.unwrap().claim_body(channel));
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    io.late_enabled.store(true, Ordering::SeqCst);
    io.queued_bodies.store(1, Ordering::SeqCst);
    let tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(
        adoption(channel),
        Adoption::Deferred,
        "the old rollout stays Legacy's"
    );
    assert!(!harness.store.has_channel_dir(channel));
    Takeover {
        harness,
        io,
        tasks,
        _reserved: reserved,
    }
}

/// Binds the next rollout once the old one's queued Legacy body went out.
fn bind_new(io: &TestIo, channel: u64, path: &Path) {
    io.queued_bodies.store(0, Ordering::SeqCst);
    bind_late(channel, path);
}

fn legacy_sends(channel: u64) -> bool {
    !o_owns_tui_output_for_channel_reserving(channel, Some(CodexTui))
        .unwrap()
        .owned
}

fn init_sources(harness: &Harness, channel: u64) -> Vec<(SourceId, u64)> {
    let init = harness.store.read_init(channel).unwrap().unwrap();
    assert_eq!(init.initial_anchor, 0);
    (init.sources.into_iter())
        .map(|source| (source.source_id, source.delivery_start))
        .collect()
}

/// A new rollout through its first prompt commits from its first byte; Legacy then sends nothing,
/// O posts the turn's answer once, a later rollout is followed and a restart reposts nothing.
#[tokio::test(start_paused = true)]
async fn a_new_rollout_before_any_output_commits_and_o_posts_its_answer_once() {
    const CODEX: u64 = 61_811;
    let Takeover {
        harness,
        io,
        tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let (path, source) = rollout(&harness, "rollout-new.jsonl", &prompted("s-codex"));
    bind_new(&io, CODEX, &path);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source.clone(), 0)]);
    let origin = harness.store.read_init_origin(CODEX).unwrap();
    assert_eq!(
        origin,
        crate::services::tui_o::store::InitOrigin::LateCodexTakeover
    );
    assert!(io.calls().contains(&("fence", CODEX)) && io.calls().contains(&("adopted", CODEX)));
    assert!(
        !legacy_sends(CODEX),
        "Legacy sends no body once O committed"
    );

    append(&path, &answer("msg_1", "the answer"));
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["the answer"],
        "{:?}",
        io.alarms.halted()
    );
    let (next, _) = rollout(&harness, "rollout-next.jsonl", &metadata("s-codex"));
    bind_new(&io, CODEX, &next);
    polls(3).await;
    append(&next, &answer("msg_2", "the next answer"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["the answer", "the next answer"]);
    restart_posts_nothing(&harness, tasks, &source).await;
}

/// A rollout bound with only its metadata stays Legacy's and keeps intake open; once its first prompt
/// lands the same rollout commits, and its answer is posted once by O.
#[tokio::test(start_paused = true)]
async fn a_metadata_only_rollout_is_taken_only_once_its_first_prompt_lands() {
    const CODEX: u64 = 61_817;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let (path, source) = rollout(&harness, "rollout-new.jsonl", &metadata("s-codex"));
    bind_new(&io, CODEX, &path);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Deferred,
        "no prompt, no takeover"
    );
    assert!(!harness.store.has_channel_dir(CODEX));
    // Not `legacy_sends`: that reserving read is itself a Legacy send and skips the rollout.
    assert!(
        crate::services::tui_o::cutover::codex_turn_awaited(CODEX).is_none(),
        "Discord input still runs on Legacy"
    );

    append(&path, &opening());
    // Growth of the same rollout is judged again on the takeover's retry wake.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, 0)]);
    assert!(crate::services::tui_o::cutover::codex_turn_awaited(CODEX).is_some());
    let end = codex_line(
        serde_json::json!({"type": "event_msg", "timestamp": Utc::now().to_rfc3339(),
        "payload": {"type": "task_complete", "turn_id": "t1"}}),
    );
    let opener = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|record| {
            record
                .pointer("/payload/type")
                .and_then(serde_json::Value::as_str)
                == Some("task_started")
        })
        .unwrap();
    let closing: serde_json::Value = serde_json::from_slice(&end).unwrap();
    let timestamp = |record: &serde_json::Value| {
        DateTime::parse_from_rfc3339(record["timestamp"].as_str().unwrap()).unwrap()
    };
    let elapsed = timestamp(&closing)
        .signed_duration_since(timestamp(&opener))
        .num_milliseconds();
    let expected = [
        "the answer".to_owned(),
        crate::services::turn_summary_line::render(
            crate::services::turn_summary_line::Outcome::Completed,
            u64::try_from(elapsed).ok(),
            None,
            None,
        ),
    ];
    append(&path, &[answer("msg_1", "the answer"), end].concat());
    polls(3).await;
    assert_eq!(harness.port.posts(), expected, "{:?}", io.alarms.halted());
    polls(3).await;
    assert_eq!(harness.port.posts(), expected);
}

/// A Legacy body sent after the baseline, finished or not, skips the rollout bound next; while that
/// rollout's turn may hold the body, the rollout bound after it waits too instead of leaving it.
#[tokio::test(start_paused = true)]
async fn a_legacy_body_after_the_baseline_skips_that_rollout_until_the_next_bind() {
    for (channel, finished) in [(61_812_u64, true), (61_813, false)] {
        let Takeover {
            harness,
            io,
            tasks: _tasks,
            _reserved,
        } = hosted_over_old(channel).await;
        let candidate =
            test_override::with_channels(|boot| boot.unwrap().candidate(channel).cloned());
        let candidate = candidate.unwrap();
        let claimed = candidate.claim_body(channel);
        assert!(!claimed.owned);
        let held = (!finished).then_some(claimed);
        let (skipped, _) = rollout(&harness, "rollout-skipped.jsonl", &prompted("s-codex"));
        bind_new(&io, channel, &skipped);
        polls(3).await;
        assert_eq!(adoption(channel), Adoption::Deferred, "finished={finished}");
        assert!(
            !harness.store.has_channel_dir(channel),
            "finished={finished}"
        );
        drop(held);

        let (taken, _) = rollout(&harness, "rollout-taken.jsonl", &prompted("s-codex"));
        bind_new(&io, channel, &taken);
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Deferred,
            "{:?}",
            io.alarms.halted()
        );
        assert!(
            !harness.store.has_channel_dir(channel),
            "finished={finished}"
        );
    }
}

/// Under the adoption lock, a Legacy send finished since the fence leaves no init for that rollout;
/// a turn answered and ended since the judgement is taken from its opener, a mirror judged again.
#[tokio::test(start_paused = true)]
async fn the_locked_recheck_refuses_a_send_or_output_that_landed_after_the_fence() {
    for (channel, race) in [(61_814_u64, "send"), (61_815, "output"), (61_816, "mirror")] {
        let Takeover {
            harness,
            io,
            tasks: _tasks,
            _reserved,
        } = hosted_over_old(channel).await;
        let (path, source) = rollout(&harness, "rollout-raced.jsonl", &prompted("s-codex"));
        let appended = path.clone();
        // The hosted activation runs on this test's thread, which holds the forced snapshot.
        test_hook::set(channel, Step::BeforeLock, move || {
            match race {
                "send" => assert!(legacy_sends(channel)),
                "output" => append(&appended, &[answer("msg_1", "raced"), turn_end()].concat()),
                _ => append(&appended, &mirror()),
            }
            Ok(())
        });
        bind_new(&io, channel, &path);
        polls(3).await;
        if race == "output" {
            assert_eq!(
                adoption(channel),
                Adoption::Committed,
                "{:?}",
                io.alarms.halted()
            );
            // The turn's own end does not move the frontier past its undelivered answer.
            let opener = metadata("s-codex").len() as u64;
            assert_eq!(init_sources(&harness, channel), [(source, opener)]);
            polls(3).await;
            let posts = ["raced".to_owned(), completed_summary(&path)];
            assert_eq!(harness.port.posts(), posts, "{:?}", io.alarms.halted());
            let late = crate::services::tui_o::cutover::claim_then_send(
                Some(crate::services::tui_o::cutover::BodyClaim::new(
                    channel,
                    Some(CodexTui),
                )),
                || async { "legacy" },
            );
            let late = late.await;
            assert_eq!(
                late,
                Ok(crate::services::tui_o::cutover::BodySend::OwnedByO)
            );
            continue;
        }
        if race == "mirror" {
            assert_eq!(
                adoption(channel),
                Adoption::Committed,
                "{:?}",
                io.alarms.halted()
            );
            assert_eq!(init_sources(&harness, channel), [(source, 0)]);
            continue;
        }
        assert_eq!(adoption(channel), Adoption::Deferred, "{race}");
        assert!(!harness.store.has_channel_dir(channel), "{race}");
        // The raced rollout's open turn may hold that send, so the next rollout does not leave it.
        let (next, _) = rollout(&harness, "rollout-after.jsonl", &prompted("s-codex"));
        bind_new(&io, channel, &next);
        polls(3).await;
        assert_eq!(adoption(channel), Adoption::Deferred, "{race}");
        assert!(!harness.store.has_channel_dir(channel), "{race}");
    }
}

/// A takeover cut off between its init and the era seal is held; the next process start seeds the
/// init committed, the host seals the era over it, recovers its store and O posts the answer once.
#[tokio::test(start_paused = true)]
async fn a_takeover_cut_off_before_its_era_seal_recovers_its_store_on_the_next_start() {
    use crate::services::tui_o::store::{InitOrigin, fault};
    const CODEX: u64 = 61_823;
    let Takeover {
        harness,
        io,
        tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let era_file = harness._runtime.path().join("o_store").join("o_era");
    let cut = fault::plant(
        &era_file,
        fault::Step::Write,
        std::io::ErrorKind::Other,
        None,
    );
    let (path, source) = rollout(&harness, "rollout-new.jsonl", &prompted("s-codex"));
    bind_new(&io, CODEX, &path);
    polls(3).await;
    drop(cut);
    assert_eq!(adoption(CODEX), Adoption::Held, "{:?}", io.alarms.halted());
    assert!(harness.store.read_era().unwrap().is_none());
    let origin = harness.store.read_init_origin(CODEX).unwrap();
    assert_eq!(origin, InitOrigin::LateCodexTakeover);
    abort(tasks);
    drop(_reserved);
    polls(1).await;

    let _rebooted = rebooted(CODEX, harness._runtime.path());
    assert_eq!(adoption(CODEX), Adoption::Committed);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let _tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    assert!(ready.is_ready(CODEX), "{:?}", io.alarms.halted());
    let era = harness.store.read_era().unwrap().unwrap();
    assert_eq!(era.initial_channels, [CODEX]);
    assert_eq!(init_sources(&harness, CODEX), [(source, 0)]);
    assert!(
        !io.calls().contains(&("facts", CODEX)) && !io.calls().contains(&("adopted", CODEX)),
        "the start recovers the store, it does not activate again"
    );
    append(&path, &answer("msg_1", "the answer"));
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["the answer"],
        "{:?}",
        io.alarms.halted()
    );
}

/// The end of turn `t1`.
fn turn_end() -> Vec<u8> {
    codex_line(
        serde_json::json!({"type": "event_msg", "timestamp": Utc::now().to_rfc3339(),
        "payload": {"type": "task_complete", "turn_id": "t1"}}),
    )
}

/// The summary O posts for `path`'s turn `t1`, timed from its opener to its end as recorded.
fn completed_summary(path: &Path) -> String {
    let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let at = |kind: &str| {
        let field = |record: &serde_json::Value, at| record.pointer(at).cloned();
        let record = records.iter().find(|record| {
            field(record, "/payload/type") == Some(kind.into())
                && field(record, "/payload/turn_id") == Some("t1".into())
        });
        DateTime::parse_from_rfc3339(record.unwrap()["timestamp"].as_str().unwrap()).unwrap()
    };
    let elapsed = at("task_complete").signed_duration_since(at("task_started"));
    crate::services::turn_summary_line::render(
        crate::services::turn_summary_line::Outcome::Completed,
        u64::try_from(elapsed.num_milliseconds()).ok(),
        None,
        None,
    )
}

/// A host over an empty binding log, started before any rollout is bound.
async fn hosted_unbound(channel: u64) -> Takeover {
    let harness = fresh_on(channel);
    harness.gate.acquired();
    let reserved = reserve(channel);
    p5::set_test_root(Some(harness._runtime.path()));
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    io.late_enabled.store(true, Ordering::SeqCst);
    let tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    Takeover {
        harness,
        io,
        tasks,
        _reserved: reserved,
    }
}

/// A resumed rollout bound after the baseline whose history already holds an answer.
fn warm(harness: &Harness, name: &str) -> (PathBuf, SourceId, u64) {
    let bytes = [metadata("s-codex"), history("t0", "old answer")].concat();
    let (path, source) = rollout(harness, name, &bytes);
    (path, source, bytes.len() as u64)
}

fn rewrite(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(from));
    std::fs::write(path, text.replace(from, to)).unwrap();
}

/// A resumed rollout that already holds answered history is taken at its end: the managed turn
/// after it gets no Legacy body, O posts it once, and a restart that rereads the init goes on.
#[tokio::test(start_paused = true)]
async fn a_warm_rollout_holding_output_is_taken_at_its_frontier_and_its_next_turn_posted_once() {
    const CODEX: u64 = 61_831;
    let Takeover {
        harness,
        io,
        tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    let (path, source, end) = warm(&harness, "rollout-warm.jsonl");
    bind_late(CODEX, &path);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source.clone(), end)]);
    assert!(!legacy_sends(CODEX), "the managed turn's body is O's");
    append(&path, &[opening(), answer("msg_1", "the answer")].concat());
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["the answer"],
        "{:?}",
        io.alarms.halted()
    );
    restart_posts_nothing(&harness, tasks, &source).await;
    append(&path, &answer("msg_2", "after the restart"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["the answer", "after the restart"]);
}

/// A byte before the frontier changed after the init was planned refuses the commit under the
/// fence: no store and nothing posted. Restored, the next wake commits the same rollout.
#[tokio::test(start_paused = true)]
async fn a_byte_changed_before_the_frontier_refuses_the_commit_until_it_is_restored() {
    const CODEX: u64 = 61_832;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    let (path, source, end) = warm(&harness, "rollout-changed.jsonl");
    let changed = path.clone();
    test_hook::set(CODEX, Step::BeforeFence, move || {
        rewrite(&changed, "old answer", "OLD answer");
        Ok(())
    });
    bind_late(CODEX, &path);
    polls(3).await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CODEX));
    assert!(harness.port.posts().is_empty());

    rewrite(&path, "OLD answer", "old answer");
    tokio::time::sleep(Duration::from_secs(5)).await;
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, end)]);
}

/// Turns opened after the bind while a Legacy terminal delivery blocks the commit, the first ended
/// and the second open, are taken from the first opener; O posts each answer once.
#[tokio::test(start_paused = true)]
async fn a_turn_opened_after_the_bind_is_taken_from_its_opener_and_posted_once() {
    const CODEX: u64 = 61_833;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    *io.custody.lock().unwrap() = Ok(Custody::Active);
    let (path, source, opened) = warm(&harness, "rollout-held.jsonl");
    bind_late(CODEX, &path);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Deferred,
        "custody blocks the commit"
    );
    let second = codex_line(serde_json::json!({"type": "event_msg",
        "timestamp": Utc::now().to_rfc3339(), "payload": {"type": "task_started", "turn_id": "t2"}}));
    let held = [opening(), answer("msg_1", "the ended answer"), turn_end()];
    append(&path, &held.concat());
    append(
        &path,
        &[second, answer("msg_2", "the held answer")].concat(),
    );
    *io.custody.lock().unwrap() = Ok(Custody::Free);
    tokio::time::sleep(Duration::from_secs(5)).await;
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, opened)]);
    polls(3).await;
    let posts = [
        "the ended answer".to_owned(),
        completed_summary(&path),
        "the held answer".to_owned(),
    ];
    assert_eq!(harness.port.posts(), posts, "{:?}", io.alarms.halted());
}

/// A rollout left with an owed open turn when the next one is bound goes into the init beside it,
/// from that turn's opener; O posts the old turn's answer and the new rollout's answer, each once.
#[tokio::test(start_paused = true)]
async fn an_owed_turn_on_the_rollout_before_a_new_bind_goes_into_the_init() {
    const CODEX: u64 = 61_834;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    *io.custody.lock().unwrap() = Ok(Custody::Active);
    let (old, old_source, opened) = warm(&harness, "rollout-before.jsonl");
    bind_late(CODEX, &old);
    polls(3).await;
    append(
        &old,
        &[opening(), answer("msg_1", "the owed answer")].concat(),
    );
    let (next, next_source) = rollout(&harness, "rollout-after.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &next);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Deferred,
        "custody blocks the commit"
    );
    *io.custody.lock().unwrap() = Ok(Custody::Free);
    tokio::time::sleep(Duration::from_secs(5)).await;
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(
        init_sources(&harness, CODEX),
        [(old_source, opened), (next_source, 0)]
    );
    append(&next, &answer("msg_2", "the new answer"));
    polls(3).await;
    let mut posts = harness.port.posts();
    posts.sort();
    assert_eq!(
        posts,
        ["the new answer", "the owed answer"],
        "{:?}",
        io.alarms.halted()
    );
}

/// Moves the time the binding log records for `channel`'s binds back by `ago`, as if its Codex
/// execution had started then.
fn bound_ago(harness: &Harness, channel: u64, ago: chrono::TimeDelta) {
    let log = (harness._runtime.path())
        .join(p5::BINDING_EVENTS_DIR)
        .join(format!("{channel}.log"));
    let at = (Utc::now() - ago).to_rfc3339();
    let lines: String = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| {
            let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
            event["committed_at"] = at.clone().into();
            format!("{event}\n")
        })
        .collect();
    std::fs::write(&log, lines).unwrap();
    p5::forget_channel_for_tests(channel);
}

/// Turn `t1` opened `opened` ago and ended `ended` ago, by Codex's clock.
fn turn_ago(opened: i64, ended: i64) -> Vec<u8> {
    let at = |secs| (Utc::now() - chrono::TimeDelta::seconds(secs)).to_rfc3339();
    [
        codex_line(
            serde_json::json!({"type": "event_msg", "timestamp": at(opened),
            "payload": {"type": "task_started", "turn_id": "t1"}}),
        ),
        codex_line(
            serde_json::json!({"type": "response_item", "timestamp": at(opened),
            "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]}}),
        ),
        answer("msg_1", "ended early"),
        codex_line(
            serde_json::json!({"type": "event_msg", "timestamp": at(ended),
            "payload": {"type": "task_complete", "turn_id": "t1"}}),
        ),
    ]
    .concat()
}

/// A turn this execution ended before the baseline, behind Legacy's inflight row with no Legacy
/// send, is posted from its opener once; with no Legacy custody left it was delivered.
#[tokio::test(start_paused = true)]
async fn a_turn_ended_before_the_baseline_is_skipped_only_on_delivery_evidence() {
    for (channel, custody) in [(61_835_u64, Custody::Row), (61_836, Custody::Free)] {
        let harness = fresh_on(channel);
        harness.gate.acquired();
        let _reserved = reserve(channel);
        p5::set_test_root(Some(harness._runtime.path()));
        let (path, source) = rollout(&harness, "rollout-ended.jsonl", &metadata("s-codex"));
        bind_late(channel, &path);
        bound_ago(&harness, channel, chrono::TimeDelta::seconds(10));
        let opener = std::fs::metadata(&path).unwrap().len();
        append(&path, &turn_ago(5, 3));
        let end = std::fs::metadata(&path).unwrap().len();
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        io.late_enabled.store(true, Ordering::SeqCst);
        *io.custody.lock().unwrap() = Ok(custody);
        let _tasks = start_codex(&harness, &io, &ready);
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        polls(3).await;
        if custody == Custody::Row {
            assert_eq!(init_sources(&harness, channel), [(source, opener)]);
            let posts = ["ended early".to_owned(), completed_summary(&path)];
            assert_eq!(harness.port.posts(), posts, "{:?}", io.alarms.halted());
        } else {
            assert_eq!(init_sources(&harness, channel), [(source, end)]);
            assert!(
                harness.port.posts().is_empty(),
                "a delivered turn is not reposted"
            );
        }
    }
}

/// A turn on the rollout bound before, opened before the baseline after a Legacy send and ended
/// undelivered, keeps a new rollout's takeover waiting; once Legacy delivered both, it commits.
#[tokio::test(start_paused = true)]
async fn an_undelivered_turn_on_the_rollout_before_holds_the_next_rollouts_takeover() {
    const CODEX: u64 = 61_837;
    let harness = fresh_on(CODEX);
    harness.gate.acquired();
    let _reserved = reserve(CODEX);
    p5::set_test_root(Some(harness._runtime.path()));
    let (old, _) = rollout(&harness, "rollout-old.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &old);
    append(&old, &[opening(), answer("msg_0", "old answer")].concat());
    let candidate = test_override::with_channels(|boot| boot.unwrap().candidate(CODEX).cloned());
    let candidate = candidate.unwrap();
    drop(candidate.claim_body(CODEX));
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    io.late_enabled.store(true, Ordering::SeqCst);
    let _tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Deferred,
        "the old turn may be Legacy's"
    );
    append(&old, &turn_end());
    let (path, source) = rollout(&harness, "rollout-new.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &path);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Deferred,
        "the new rollout waits on the old turn"
    );
    assert!(!harness.store.has_channel_dir(CODEX));

    append(
        &path,
        &[answer("msg_1", "the new answer"), turn_end()].concat(),
    );
    drop(candidate.claim_body(CODEX));
    tokio::time::sleep(Duration::from_secs(5)).await;
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    let end = std::fs::metadata(&path).unwrap().len();
    assert_eq!(init_sources(&harness, CODEX), [(source, end)]);
    assert!(
        harness.port.posts().is_empty(),
        "Legacy delivered both turns"
    );
}

/// A turn whose records carry no time cannot be told from this execution's, so the takeover waits
/// instead of starting past it at the end of the file.
#[tokio::test(start_paused = true)]
async fn a_turn_without_a_time_keeps_the_takeover_waiting() {
    const CODEX: u64 = 61_838;
    let Takeover {
        harness,
        io: _io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    let (path, _) = rollout(&harness, "rollout-undated.jsonl", &metadata("s-codex"));
    let undated = [
        codex_line(serde_json::json!({"type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "t1"}})),
        codex_line(serde_json::json!({"type": "response_item",
            "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]}})),
        codex_line(serde_json::json!({"type": "response_item",
            "payload": {"type": "message", "role": "assistant", "id": "msg_1",
            "content": [{"type": "output_text", "text": "undated answer"}]}})),
    ];
    append(&path, &undated.concat());
    bind_late(CODEX, &path);
    polls(3).await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CODEX));
}
