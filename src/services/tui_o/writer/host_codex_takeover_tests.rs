//! A reserved Codex channel taken over through the real host: a fresh rollout from its first byte,
//! one that holds output from its frontier, and Legacy sends nothing once it commits.

use super::*;
use crate::config::Config;
use crate::services::tui_o::writer::adoption::{
    CodexDelivered, CodexDelivery, LegacyCursor, LegacyView,
};
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

/// A host whose channel already bound a rollout whose one turn Legacy answered and recorded before
/// the baseline; a queued Legacy body holds every commit until `bind_new` binds the next rollout.
async fn hosted_over_old(channel: u64) -> Takeover {
    let harness = fresh_on(channel);
    harness.gate.acquired();
    let reserved = reserve(channel);
    p5::set_test_root(Some(harness._runtime.path()));
    let (old, _) = rollout(&harness, "rollout-old.jsonl", &metadata("s-codex"));
    bind_late(channel, &old);
    let answered = [opening(), answer("msg_0", "old answer"), turn_end()];
    let opener = len(&old);
    append(&old, &answered.concat());
    let candidate = test_override::with_channels(|boot| boot.unwrap().candidate(channel).cloned());
    drop(candidate.unwrap().claim_body(channel));
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    recorded(&io, vec![receipt((opener, len(&old)))]);
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

/// Legacy holding Codex delivery records of a tmux generation.
struct Records(CodexDelivered);

impl LegacyView for Records {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        LegacyCursor::Unbound
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        None
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }

    fn codex_delivered(&self, _: u64, _: &str) -> Result<CodexDelivered, String> {
        Ok(self.0.clone())
    }
}

/// Legacy's receipt for `range`, dated now.
fn receipt(range: (u64, u64)) -> CodexDelivery {
    let at = Some(Utc::now());
    CodexDelivery { range, at }
}

/// Legacy's records are `receipts`, of a generation that began an hour ago.
fn recorded(io: &TestIo, receipts: Vec<CodexDelivery>) {
    recorded_since(io, Utc::now() - chrono::TimeDelta::hours(1), receipts);
}

fn recorded_since(io: &TestIo, since: DateTime<Utc>, receipts: Vec<CodexDelivery>) {
    let generation_since = Some(since);
    let records = CodexDelivered {
        generation_since,
        frontier: None,
        receipts,
    };
    *io.legacy.lock().unwrap() = Some(Arc::new(Records(records)));
}

fn len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// Where a fresh rollout's first turn opens.
fn first_opener() -> u64 {
    metadata("s-codex").len() as u64
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

/// A resumed rollout bound after the baseline whose history already holds an answer; Legacy's
/// record of that answer is set by `history_recorded` once it is bound.
fn warm(harness: &Harness, name: &str) -> (PathBuf, SourceId, u64) {
    let bytes = [metadata("s-codex"), history("t0", "old answer")].concat();
    let (path, source) = rollout(harness, name, &bytes);
    (path, source, bytes.len() as u64)
}

fn history_recorded(io: &TestIo, end: u64) {
    recorded(io, vec![receipt((first_opener(), end))]);
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
    history_recorded(&io, end);
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
    history_recorded(&io, end);
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
    history_recorded(&io, opened);
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
    history_recorded(&io, opened);
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

/// A turn this execution ended before the baseline with no Legacy send is posted from its opener
/// once, behind Legacy's row or with no custody at all; only Legacy's record of it leaves it out.
#[tokio::test(start_paused = true)]
async fn a_turn_ended_before_the_baseline_is_skipped_only_on_delivery_evidence() {
    let cases = [
        (61_835_u64, Custody::Row, false),
        (61_836, Custody::Free, false),
        (61_870, Custody::Free, true),
    ];
    for (channel, custody, delivered) in cases {
        let harness = fresh_on(channel);
        harness.gate.acquired();
        let _reserved = reserve(channel);
        p5::set_test_root(Some(harness._runtime.path()));
        let (path, source) = rollout(&harness, "rollout-ended.jsonl", &metadata("s-codex"));
        bind_late(channel, &path);
        bound_ago(&harness, channel, chrono::TimeDelta::seconds(10));
        let opener = len(&path);
        append(&path, &turn_ago(5, 3));
        let end = len(&path);
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        io.late_enabled.store(true, Ordering::SeqCst);
        *io.custody.lock().unwrap() = Ok(custody);
        if delivered {
            recorded(&io, vec![receipt((opener, end))]);
        }
        let _tasks = start_codex(&harness, &io, &ready);
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        polls(3).await;
        if delivered {
            assert_eq!(init_sources(&harness, channel), [(source, end)]);
            assert!(
                harness.port.posts().is_empty(),
                "a recorded turn is not reposted"
            );
        } else {
            assert_eq!(init_sources(&harness, channel), [(source, opener)]);
            let posts = ["ended early".to_owned(), completed_summary(&path)];
            assert_eq!(harness.port.posts(), posts, "{:?}", io.alarms.halted());
        }
    }
}

/// A turn on the rollout bound before, opened before the baseline after a Legacy send and ended
/// undelivered, keeps a new rollout's takeover waiting; once Legacy recorded both, it commits.
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
    let old_delivered = receipt((first_opener(), len(&old)));
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
    recorded(&io, vec![old_delivered, receipt((0, len(&path)))]);
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

/// A turn whose records carry no time is taken from its opener when uncertain; only Legacy's record
/// of it leaves it out.
#[tokio::test(start_paused = true)]
async fn a_turn_without_a_time_is_taken_unless_legacy_records_cover_it() {
    for (channel, delivered) in [(61_838_u64, false), (61_871, true)] {
        let Takeover {
            harness,
            io,
            tasks: _tasks,
            _reserved,
        } = hosted_unbound(channel).await;
        let (path, source) = rollout(&harness, "rollout-undated.jsonl", &metadata("s-codex"));
        let opener = len(&path);
        let mut undated = vec![
            codex_line(serde_json::json!({"type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "t1"}})),
            codex_line(serde_json::json!({"type": "response_item",
                "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]}})),
            codex_line(serde_json::json!({"type": "response_item",
                "payload": {"type": "message", "role": "assistant", "id": "msg_1",
                "content": [{"type": "output_text", "text": "undated answer"}]}})),
        ];
        if delivered {
            undated.push(codex_line(serde_json::json!({"type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "t1"}})));
        }
        append(&path, &undated.concat());
        bind_late(channel, &path);
        if delivered {
            recorded(&io, vec![receipt((opener, len(&path)))]);
        }
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        polls(3).await;
        if delivered {
            assert_eq!(init_sources(&harness, channel), [(source, len(&path))]);
            assert!(harness.port.posts().is_empty());
        } else {
            assert_eq!(init_sources(&harness, channel), [(source, opener)]);
            assert_eq!(harness.port.posts(), ["undated answer"]);
        }
    }
}

/// Turn `id` answered `text`, opened `opened` and ended `ended` seconds ago by Codex's clock.
fn dated_turn(id: &str, text: &str, opened: i64, ended: i64) -> Vec<u8> {
    let at = |secs| (Utc::now() - chrono::TimeDelta::seconds(secs)).to_rfc3339();
    let message = format!("msg_{id}");
    [
        codex_line(
            serde_json::json!({"type": "event_msg", "timestamp": at(opened),
            "payload": {"type": "task_started", "turn_id": id}}),
        ),
        codex_line(
            serde_json::json!({"type": "response_item", "timestamp": at(opened),
            "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]}}),
        ),
        answer(&message, text),
        codex_line(
            serde_json::json!({"type": "event_msg", "timestamp": at(ended),
            "payload": {"type": "task_complete", "turn_id": id}}),
        ),
    ]
    .concat()
}

fn posted(harness: &Harness, text: &str) -> usize {
    harness
        .port
        .posts()
        .iter()
        .filter(|post| *post == text)
        .count()
}

/// Two turns ended before the baseline, the first behind Legacy's row and the second behind a
/// pending start, with no Legacy record, are both taken from the first opener; a record of the
/// first leaves only the second.
#[tokio::test(start_paused = true)]
async fn two_ended_turns_without_a_legacy_record_are_both_taken() {
    for (channel, first_delivered) in [(61_872_u64, false), (61_873, true)] {
        let harness = fresh_on(channel);
        harness.gate.acquired();
        let _reserved = reserve(channel);
        p5::set_test_root(Some(harness._runtime.path()));
        let (path, source) = rollout(&harness, "rollout-two.jsonl", &metadata("s-codex"));
        bind_late(channel, &path);
        bound_ago(&harness, channel, chrono::TimeDelta::seconds(10));
        let first = len(&path);
        append(&path, &dated_turn("t1", "first answer", 8, 6));
        let second = len(&path);
        append(&path, &dated_turn("t2", "second answer", 5, 3));
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        io.late_enabled.store(true, Ordering::SeqCst);
        *io.custody.lock().unwrap() = Ok(Custody::PendingStart);
        if first_delivered {
            recorded(&io, vec![receipt((first, second))]);
        }
        let _tasks = start_codex(&harness, &io, &ready);
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        polls(3).await;
        let start = if first_delivered { second } else { first };
        assert_eq!(init_sources(&harness, channel), [(source, start)]);
        assert_eq!(
            posted(&harness, "first answer"),
            usize::from(!first_delivered)
        );
        assert_eq!(posted(&harness, "second answer"), 1);
    }
}

/// A rollout left with an unrecorded ended turn, then two more bound before the commit, stays in
/// the init from that turn's opener; Legacy's record of the turn leaves it out.
#[tokio::test(start_paused = true)]
async fn a_rotation_through_two_rollouts_keeps_the_first_rollouts_turn() {
    for (channel, delivered) in [(61_874_u64, false), (61_875, true)] {
        let Takeover {
            harness,
            io,
            tasks: _tasks,
            _reserved,
        } = hosted_unbound(channel).await;
        *io.custody.lock().unwrap() = Ok(Custody::Active);
        let (a, a_source) = rollout(&harness, "rollout-a.jsonl", &metadata("s-codex"));
        bind_late(channel, &a);
        let opener = len(&a);
        let first = [
            opening(),
            answer("msg_1", "first rollout answer"),
            turn_end(),
        ];
        append(&a, &first.concat());
        if delivered {
            recorded(&io, vec![receipt((opener, len(&a)))]);
        }
        polls(3).await;
        let (b, b_source) = rollout(&harness, "rollout-b.jsonl", &prompted("s-codex"));
        bind_late(channel, &b);
        polls(3).await;
        let (c, c_source) = rollout(&harness, "rollout-c.jsonl", &prompted("s-codex"));
        bind_late(channel, &c);
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Deferred,
            "custody blocks the commit"
        );
        *io.custody.lock().unwrap() = Ok(Custody::Free);
        tokio::time::sleep(Duration::from_secs(5)).await;
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        let mut expected = vec![(b_source, first_opener()), (c_source, 0)];
        if !delivered {
            expected.insert(0, (a_source, opener));
        }
        assert_eq!(init_sources(&harness, channel), expected);
        polls(3).await;
        assert_eq!(
            posted(&harness, "first rollout answer"),
            usize::from(!delivered)
        );
    }
}

/// A rollout judged owed stays in the init once the generation Legacy's records belong to began
/// after its bind ended.
#[tokio::test(start_paused = true)]
async fn a_rollout_judged_owed_stays_owed_after_the_generation_moves_past_it() {
    const CODEX: u64 = 61_876;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    *io.custody.lock().unwrap() = Ok(Custody::Active);
    let (a, a_source) = rollout(&harness, "rollout-a.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &a);
    let opener = len(&a);
    append(
        &a,
        &[opening(), answer("msg_1", "owed answer"), turn_end()].concat(),
    );
    polls(3).await;
    let (b, b_source) = rollout(&harness, "rollout-b.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &b);
    polls(3).await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    recorded_since(&io, Utc::now(), Vec::new());
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
        [(a_source, opener), (b_source, 0)]
    );
}

/// A receipt dated while another rollout of the same generation was bound does not cover this
/// rollout's turn at the very same bytes.
#[tokio::test(start_paused = true)]
async fn a_receipt_of_another_rollout_in_the_generation_does_not_cover_this_one() {
    const CODEX: u64 = 61_877;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    *io.custody.lock().unwrap() = Ok(Custody::Active);
    let turn = [opening(), answer("msg_1", "same bytes"), turn_end()].concat();
    let (a, _) = rollout(&harness, "rollout-a.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &a);
    let opener = len(&a);
    append(&a, &turn);
    let a_delivered = receipt((opener, len(&a)));
    polls(3).await;
    let (b, b_source) = rollout(&harness, "rollout-b.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &b);
    append(&b, &turn);
    assert_eq!(len(&a), len(&b));
    recorded(&io, vec![a_delivered]);
    *io.custody.lock().unwrap() = Ok(Custody::Free);
    tokio::time::sleep(Duration::from_secs(5)).await;
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(b_source, opener)]);
    polls(3).await;
    assert_eq!(posted(&harness, "same bytes"), 1);
}

/// Rewrites `channel`'s one bind as a Pending bound `ago` and resolved now.
fn pend_then_resolve(harness: &Harness, channel: u64, ago: chrono::TimeDelta) {
    let log = (harness._runtime.path())
        .join(p5::BINDING_EVENTS_DIR)
        .join(format!("{channel}.log"));
    let text = std::fs::read_to_string(&log).unwrap();
    let bound: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let source = bound["new"]["source"].clone();
    let mut pending = bound.clone();
    pending["new"] = serde_json::json!({"pending": {"payload_session_id": "s-codex",
        "payload_transcript_path": source["path"].clone()}});
    pending["committed_at"] = (Utc::now() - ago).to_rfc3339().into();
    let mut resolved = bound;
    resolved["seq"] = 2.into();
    resolved["new"] = serde_json::json!({"resolved": {"pending_seq": 1, "source": source}});
    std::fs::write(&log, format!("{pending}\n{resolved}\n")).unwrap();
    p5::forget_channel_for_tests(channel);
}

/// A turn Codex opened before its rollout's bind was recorded, written again late or resolving a
/// Pending late, is taken from its opener when no Legacy record covers it.
#[tokio::test(start_paused = true)]
async fn a_turn_opened_before_a_late_bind_is_taken_without_a_legacy_record() {
    for (channel, pending) in [(61_878_u64, false), (61_879, true)] {
        let harness = fresh_on(channel);
        harness.gate.acquired();
        let _reserved = reserve(channel);
        p5::set_test_root(Some(harness._runtime.path()));
        let (path, source) = rollout(&harness, "rollout-late.jsonl", &metadata("s-codex"));
        let opener = len(&path);
        append(&path, &turn_ago(5, 3));
        bind_late(channel, &path);
        if pending {
            pend_then_resolve(&harness, channel, chrono::TimeDelta::seconds(8));
        }
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        io.late_enabled.store(true, Ordering::SeqCst);
        let _tasks = start_codex(&harness, &io, &ready);
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "pending={pending} {:?}",
            io.alarms.halted()
        );
        assert_eq!(init_sources(&harness, channel), [(source, opener)]);
        polls(3).await;
        let posts = ["ended early".to_owned(), completed_summary(&path)];
        assert_eq!(harness.port.posts(), posts, "{:?}", io.alarms.halted());
    }
}

/// A rollout Legacy recorded in full does not end the walk: an older rollout of the generation with
/// an unrecorded turn still goes into the init from that turn's opener.
#[tokio::test(start_paused = true)]
async fn a_recorded_rollout_does_not_end_the_walk_before_an_older_owed_one() {
    const CODEX: u64 = 61_880;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    *io.custody.lock().unwrap() = Ok(Custody::Active);
    let (a, a_source) = rollout(&harness, "rollout-a.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &a);
    let a_opener = len(&a);
    append(
        &a,
        &[opening(), answer("msg_1", "owed answer"), turn_end()].concat(),
    );
    polls(3).await;
    let (b, _) = rollout(&harness, "rollout-b.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &b);
    let b_opener = len(&b);
    append(
        &b,
        &[opening(), answer("msg_2", "recorded answer"), turn_end()].concat(),
    );
    let b_delivered = receipt((b_opener, len(&b)));
    polls(3).await;
    let (c, c_source) = rollout(&harness, "rollout-c.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &c);
    recorded(&io, vec![b_delivered]);
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
        [(a_source, a_opener), (c_source, 0)]
    );
    polls(3).await;
    assert_eq!(posted(&harness, "owed answer"), 1);
    assert_eq!(posted(&harness, "recorded answer"), 0);
}

/// A record whose start is not a record end of the rollout is another file's bytes: it covers no
/// turn here, which is taken from its opener.
#[tokio::test(start_paused = true)]
async fn a_record_off_the_rollouts_record_ends_covers_no_turn() {
    const CODEX: u64 = 61_881;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_unbound(CODEX).await;
    let (path, source) = rollout(&harness, "rollout-off.jsonl", &metadata("s-codex"));
    let opener = len(&path);
    append(
        &path,
        &[opening(), answer("msg_1", "off answer"), turn_end()].concat(),
    );
    bind_late(CODEX, &path);
    recorded(&io, vec![receipt((opener + 1, len(&path)))]);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, opener)]);
    polls(3).await;
    assert_eq!(posted(&harness, "off answer"), 1);
}
