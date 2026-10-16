//! A reserved Codex channel taken over through the real host: only a rollout bound after the
//! baseline whose bytes still hold no output commits, and Legacy sends nothing once it does.

use super::*;
use crate::config::Config;
use std::collections::BTreeMap;

/// The boot snapshot of `channel` reserved for late adoption and selected for turn mode.
fn reserve(channel: u64) -> test_override::ChannelsGuard {
    let config: Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": channel.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [channel], "late_adopt": true},
            "turn": {"channels": [channel]}}
    }))
    .unwrap();
    let pending = |_: &_, _| Ok(BTreeMap::from([(channel, Adoption::Pending)]));
    let boot = BootChannels::validate(&config).unwrap();
    let boot = boot.seeded(true, &config, pending).unwrap();
    assert!(boot.candidate(channel).unwrap().late_adopt_origin());
    test_override::force_boot(boot)
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

/// A host whose channel already bound a rollout Legacy answered from before the baseline.
async fn hosted_over_old(channel: u64) -> Takeover {
    let harness = fresh_on(channel);
    harness.gate.acquired();
    let reserved = reserve(channel);
    p5::set_test_root(Some(harness._runtime.path()));
    let old = [metadata("s-codex"), answer("msg_0", "old answer")].concat();
    let (old, _) = rollout(&harness, "rollout-old.jsonl", &old);
    bind_late(channel, &old);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    io.late_enabled.store(true, Ordering::SeqCst);
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
    bind_late(CODEX, &path);
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source.clone(), 0)]);
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
    bind_late(CODEX, &next);
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
    bind_late(CODEX, &path);
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
    append(&path, &[answer("msg_1", "the answer"), end].concat());
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["the answer"],
        "{:?}",
        io.alarms.halted()
    );
    polls(3).await;
    assert_eq!(harness.port.posts(), ["the answer"]);
}

/// A Legacy body sent after the baseline, finished or not, skips the rollout bound next without
/// taking a new baseline on it; the rollout bound after that commits.
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
        bind_late(channel, &skipped);
        polls(3).await;
        assert_eq!(adoption(channel), Adoption::Deferred, "finished={finished}");
        assert!(
            !harness.store.has_channel_dir(channel),
            "finished={finished}"
        );
        drop(held);

        let (taken, source) = rollout(&harness, "rollout-taken.jsonl", &prompted("s-codex"));
        bind_late(channel, &taken);
        polls(3).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        assert_eq!(init_sources(&harness, channel), [(source, 0)]);
    }
}

/// Under the adoption lock, a Legacy send finished since the fence or an output appended since the
/// judgement leaves no init for that rollout; a prompt mirror appended since is judged again at once.
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
                "output" => append(&appended, &answer("msg_1", "raced")),
                _ => append(&appended, &mirror()),
            }
            Ok(())
        });
        bind_late(channel, &path);
        polls(3).await;
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
        let (next, next_source) = rollout(&harness, "rollout-after.jsonl", &prompted("s-codex"));
        bind_late(channel, &next);
        polls(3).await;
        assert_eq!(adoption(channel), Adoption::Committed, "{race}");
        assert_eq!(init_sources(&harness, channel), [(next_source, 0)]);
    }
}
