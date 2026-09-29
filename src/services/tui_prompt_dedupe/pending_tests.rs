use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::hook_server::adoption_retry::{
    AdoptionHttp, DurableKind, NotDurableReason, adopt_from_hook, deferred_adoption_count,
    reset_deferred_adoptions_for_tests,
};
use crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions;
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::binding_context::{
    observe_spawn_nonce_marker, tests::fixture,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    APPEND_FAULT, BINDING_EVENTS_DIR, forget_channel_for_tests, records_strict, set_test_root,
};
use crate::services::tui_prompt_dedupe::{
    TEST_LOCK, TuiRuntimeBinding, adopt_claude_continuation_session, clear_claude_session_rotation,
    lock_claude_session_rotations_for_tests, register_launched_tmux_runtime_binding,
    register_provider_session, register_rehydrated_tmux_runtime_binding, register_tmux_channel,
    reset_state_for_tests, resolve_tmux_session_name, runtime_binding_for_tmux_session,
};
use std::fs;
use std::sync::MutexGuard;

/// Real writers on a scratch log root and spawn-marker root, with the dedupe state serialised.
struct Lane {
    root: tempfile::TempDir,
    dir: tempfile::TempDir,
    _rotations: MutexGuard<'static, ()>,
    _state: MutexGuard<'static, ()>,
    _env: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
}

impl Lane {
    fn new() -> Self {
        let env = fixture();
        let state = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let rotations = lock_claude_session_rotations_for_tests();
        let root = tempfile::tempdir().unwrap();
        set_test_root(Some(root.path()));
        restart(0);
        let dir = tempfile::tempdir().unwrap();
        Self {
            root,
            dir,
            _rotations: rotations,
            _state: state,
            _env: env,
        }
    }

    fn path(&self, session: &str) -> PathBuf {
        self.dir.path().join(format!("{session}.jsonl"))
    }

    fn touch(&self, session: &str) -> PathBuf {
        let path = self.path(session);
        fs::write(&path, b"{}\n").unwrap();
        path
    }

    fn log(&self, channel: u64) -> PathBuf {
        self.root
            .path()
            .join(BINDING_EVENTS_DIR)
            .join(format!("{channel}.log"))
    }

    /// Replaces the complete line `line` (from 1), or drops it when `with` is `None`.
    fn edit_line(&self, channel: u64, line: usize, with: Option<&str>) -> String {
        let text = fs::read_to_string(self.log(channel)).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        let original = lines[line - 1].to_owned();
        match with {
            Some(with) => lines[line - 1] = with,
            None => {
                lines.remove(line - 1);
            }
        }
        fs::write(self.log(channel), lines.join("\n") + "\n").unwrap();
        original
    }

    fn judge(&self, channel: u64, tmux: &str, launch: &str) -> RestoreStep {
        let launch = LaunchTranscript {
            session_id: launch.to_owned(),
            transcript: self.path(launch),
        };
        let marker = observe_spawn_nonce_marker(tmux);
        judge_restore(tmux, records_strict(channel), &marker, Some(&launch), |p| {
            p.is_file()
        })
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        APPEND_FAULT.with(|fault| fault.set(None));
        restart(0);
        set_test_root(None);
    }
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Writes a fresh spawn-nonce marker for `tmux`, as a (re)spawn of the pane does.
fn stamp(tmux: &str) -> String {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    fs::write(tc::session_temp_path(tmux, "spawn_nonce"), &nonce).unwrap();
    nonce
}

/// What a dcserver restart forgets: every in-memory binding and the cached log writer.
fn restart(channel: u64) {
    forget_channel_for_tests(channel);
    reset_state_for_tests();
    reset_deferred_adoptions_for_tests();
}

fn claude(path: &Path, session: &str) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_owned()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

fn clear(transcript: &Path) -> HookSignal {
    let payload = serde_json::json!({ "source": "clear", "transcript_path": transcript });
    HookSignal::from_payload("session_start", &payload)
}

/// Launch A, then a /clear to B before B's transcript exists; the log ends in Pending{B}.
fn launch_then_clear(lane: &Lane, channel: u64, tmux: &str, a_exists: bool) -> (String, String) {
    let (a, b) = (uuid(), uuid());
    if a_exists {
        lane.touch(&a);
    }
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &a, tmux);
    register_launched_tmux_runtime_binding(tmux, claude(&lane.path(&a), &a));
    let adopted = adopt_claude_continuation_session(&a, &b, &clear(&lane.path(&b)));
    assert!(
        adopted.unwrap().is_none(),
        "B is not adopted before its transcript exists"
    );
    let records = records_strict(channel).unwrap().unwrap();
    assert!(matches!(
        records.last().unwrap().new,
        BindingTarget::Pending { .. }
    ));
    (a, b)
}

fn seed(step: RestoreStep) -> LaunchSeed {
    match step {
        RestoreStep::SeedAfterLaunch(seed) => seed,
        other => panic!("expected a launch seed, got {other:?}"),
    }
}

fn exact(step: RestoreStep) -> ExactBinding {
    match step {
        RestoreStep::PublishExact(exact) => exact,
        other => panic!("expected an exact publish, got {other:?}"),
    }
}

const REGISTERED: Registration = Registration {
    binding: true,
    command_alias: true,
};

#[test]
fn strict_read_blocks_an_unparseable_or_out_of_sequence_line() {
    let lane = Lane::new();
    let (channel, tmux) = (7_501, "p2b-strict");
    stamp(tmux);
    let (a, _) = launch_then_clear(&lane, channel, tmux, true);
    register_tmux_channel("p2b-strict-2", channel);
    register_launched_tmux_runtime_binding("p2b-strict-2", claude(&lane.touch(&uuid()), "x"));
    restart(channel);

    let pending_line = lane.edit_line(channel, 2, Some("{\"seq\":2,\"channel_id\""));
    let corrupt =
        |line, kind| RestoreStep::Finished(PendingRestore::BlockedCorrupt(Corrupt { line, kind }));
    assert_eq!(
        lane.judge(channel, tmux, &a),
        corrupt(2, CorruptKind::Unparseable),
        "unparseable Pending line blocks"
    );

    lane.edit_line(channel, 2, Some(&pending_line));
    lane.edit_line(channel, 2, None);
    let gap = CorruptKind::SeqGap {
        expected: 2,
        found: 3,
    };
    assert_eq!(
        lane.judge(channel, tmux, &a),
        corrupt(2, gap),
        "a dropped parseable line blocks"
    );
}

#[test]
fn repaired_log_is_judged_again_and_seeds_from_launch() {
    let lane = Lane::new();
    let (channel, tmux) = (7_502, "p2b-repair");
    stamp(tmux);
    let (a, b) = launch_then_clear(&lane, channel, tmux, true);
    restart(channel);
    let pending_line = lane.edit_line(channel, 2, Some("not json"));
    let RestoreStep::Finished(blocked) = lane.judge(channel, tmux, &a) else {
        panic!("a corrupt log finishes the restore");
    };
    assert!(matches!(blocked, PendingRestore::BlockedCorrupt(_)));
    assert!(!blocked.memo(), "a blocked log is judged again");

    lane.edit_line(channel, 2, Some(&pending_line));
    let seed = seed(lane.judge(channel, tmux, &a));
    assert_eq!(
        (seed.pending_seq, seed.payload_session_id.as_str()),
        (2, b.as_str())
    );
    assert_eq!(seed.launch.session_id, a);
    assert_eq!(
        seed.hook.cause(),
        BindingCause::Clear,
        "the seeded hook keeps the recorded cause"
    );
}

#[test]
fn torn_tail_is_not_corruption() {
    let lane = Lane::new();
    let (channel, tmux) = (7_503, "p2b-torn");
    stamp(tmux);
    let (a, b) = launch_then_clear(&lane, channel, tmux, true);
    restart(channel);
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(lane.log(channel))
        .unwrap();
    std::io::Write::write_all(&mut log, b"{\"seq\":3,\"chan").unwrap();

    assert_eq!(records_strict(channel).unwrap().unwrap().len(), 2);
    assert_eq!(seed(lane.judge(channel, tmux, &a)).payload_session_id, b);
}

#[test]
fn pending_of_another_execution_is_not_restored() {
    let lane = Lane::new();
    let (channel, tmux) = (7_504, "p2b-nonce");
    let nonce = stamp(tmux);
    let (a, b) = launch_then_clear(&lane, channel, tmux, true);
    restart(channel);

    stamp(tmux);
    let skip = RestoreStep::Finished(PendingRestore::NotEligible(NotEligible::NonceMismatch));
    assert_eq!(
        lane.judge(channel, tmux, &a),
        skip,
        "a respawned pane does not restore"
    );
    fs::write(tc::session_temp_path(tmux, "spawn_nonce"), &nonce).unwrap();
    assert_eq!(seed(lane.judge(channel, tmux, &a)).payload_session_id, b);
}

#[test]
fn pending_replaced_by_a_hooked_source_stays_superseded_after_its_file_appears() {
    let lane = Lane::new();
    let (channel, tmux) = (7_505, "p2b-superseded");
    stamp(tmux);
    let (a, b) = (uuid(), uuid());
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &a, tmux);
    register_launched_tmux_runtime_binding(tmux, claude(&lane.path(&a), &a));
    let b_path = lane.touch(&b);
    assert!(
        adopt_claude_continuation_session(&a, &b, &clear(&b_path))
            .unwrap()
            .is_some()
    );
    lane.touch(&a);
    restart(channel);
    let before = fs::read(lane.log(channel)).unwrap();

    let outcome = PendingRestore::NotEligible(NotEligible::Superseded);
    assert_eq!(
        lane.judge(channel, tmux, &a),
        RestoreStep::Finished(outcome.clone()),
        "Pending A superseded by Source B"
    );
    assert!(outcome.memo());
    assert_eq!(
        fs::read(lane.log(channel)).unwrap(),
        before,
        "judging writes nothing"
    );
}

#[test]
fn launch_seed_needs_the_launch_binding_and_its_alias_first() {
    let lane = Lane::new();
    let (channel, tmux) = (7_506, "p2b-seed");
    stamp(tmux);
    let (a, _) = launch_then_clear(&lane, channel, tmux, true);
    restart(channel);
    let seed = seed(lane.judge(channel, tmux, &a));
    let register = || {
        let binding = register_rehydrated_tmux_runtime_binding(
            "claude",
            tmux,
            channel,
            claude(&lane.path(&a), &a),
        );
        let command_alias = resolve_tmux_session_name("claude", &a).as_deref() == Some(tmux);
        Registration {
            binding,
            command_alias,
        }
    };

    APPEND_FAULT.with(|fault| fault.set(Some("reload")));
    let failed = seed.outcome(register());
    let down = PendingRestore::Unavailable(Unavailable::NotRegistered);
    assert_eq!(
        failed, down,
        "no seed before the launch binding is registered"
    );
    assert!(!failed.memo());
    APPEND_FAULT.with(|fault| fault.set(None));
    let no_alias = Registration {
        command_alias: false,
        ..REGISTERED
    };
    assert_eq!(
        seed.outcome(no_alias),
        down,
        "no seed without the launch alias"
    );
    let seeded = seed.outcome(register());
    assert_eq!(seeded, PendingRestore::Seeded { pending_seq: 2 });
    assert!(seeded.memo());
}

#[test]
fn file_less_exact_binding_waits_for_that_path_only() {
    let lane = Lane::new();
    let (channel, tmux) = (7_507, "p2b-exact");
    stamp(tmux);
    let (a, b) = launch_then_clear(&lane, channel, tmux, false);
    restart(channel);

    let waiting = exact(lane.judge(channel, tmux, &a)).outcome(REGISTERED);
    let wait = ExactPathWait {
        session_id: b.clone(),
        transcript: lane.path(&b),
    };
    let expected = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: Some(wait),
    };
    assert_eq!(
        waiting, expected,
        "a file-less B binds to its exact path only"
    );
    assert!(!waiting.memo());
    let unregistered = Registration {
        binding: false,
        ..REGISTERED
    };
    let down = PendingRestore::Unavailable(Unavailable::NotRegistered);
    assert_eq!(
        exact(lane.judge(channel, tmux, &a)).outcome(unregistered),
        down
    );

    lane.touch(&b);
    let bound = exact(lane.judge(channel, tmux, &a)).outcome(REGISTERED);
    assert_eq!(
        bound,
        PendingRestore::BoundFromLedger {
            pending_seq: 2,
            exact_wait: None
        }
    );
    assert!(bound.memo());
}

#[test]
fn a_transcript_gone_after_the_judgment_is_waited_for() {
    let lane = Lane::new();
    let (channel, tmux) = (7_518, "p2b-vanished");
    stamp(tmux);
    let (a, b) = launch_then_clear(&lane, channel, tmux, false);
    restart(channel);
    lane.touch(&b);
    let exact = exact(lane.judge(channel, tmux, &a));
    fs::remove_file(lane.path(&b)).unwrap();
    let wait = ExactPathWait {
        session_id: b.clone(),
        transcript: lane.path(&b),
    };
    let expected = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: Some(wait),
    };
    assert_eq!(
        exact.outcome(REGISTERED),
        expected,
        "a vanished B is waited for"
    );
}

#[test]
fn resolved_pending_restores_exactly_that_session_on_the_next_restart() {
    let lane = Lane::new();
    let resolve = |channel: u64, tmux: &str, respawn: bool| {
        stamp(tmux);
        let (a, b) = launch_then_clear(&lane, channel, tmux, false);
        restart(channel);
        if respawn {
            stamp(tmux);
        }
        let b_path = lane.touch(&b);
        assert!(register_rehydrated_tmux_runtime_binding(
            "claude",
            tmux,
            channel,
            claude(&b_path, &b)
        ));
        let records = records_strict(channel).unwrap().unwrap();
        assert!(matches!(
            records[2].new,
            BindingTarget::Resolved { pending_seq: 2, .. }
        ));
        restart(channel);
        (a, b)
    };

    let (channel, tmux) = (7_508, "p2b-resolved");
    let (a, b) = resolve(channel, tmux, false);
    let exact = exact(lane.judge(channel, tmux, &a));
    assert_eq!(
        (exact.session_id.as_str(), exact.transcript.clone()),
        (b.as_str(), lane.path(&b)),
        "Resolved B restores B"
    );
    assert_eq!(exact.launch_session_id, a);
    let bound = exact.outcome(REGISTERED);
    assert_eq!(
        bound,
        PendingRestore::BoundFromLedger {
            pending_seq: 2,
            exact_wait: None
        }
    );
    fs::remove_file(lane.path(&b)).unwrap();
    let waiting = self::exact(lane.judge(channel, tmux, &a)).outcome(REGISTERED);
    let wait = ExactPathWait {
        session_id: b.clone(),
        transcript: lane.path(&b),
    };
    let expected = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: Some(wait),
    };
    assert_eq!(waiting, expected, "a missing Resolved B waits for B");
    assert!(!waiting.memo(), "and is judged again");
    lane.touch(&b);
    let back = self::exact(lane.judge(channel, tmux, &a)).outcome(REGISTERED);
    assert_eq!(back, bound, "B comes back without a new log record");

    let (channel, tmux) = (7_509, "p2b-resolved-respawn");
    let (a, _) = resolve(channel, tmux, true);
    let skip = RestoreStep::Finished(PendingRestore::NotEligible(NotEligible::NonceMismatch));
    assert_eq!(
        lane.judge(channel, tmux, &a),
        skip,
        "a Resolved from another execution is not restored"
    );
}

#[test]
fn pending_without_a_current_execution_is_never_restored() {
    let lane = Lane::new();
    let (channel, tmux) = (7_510, "p2b-legacy");
    let healthy = RestoreStep::Finished(PendingRestore::HealthyNoPending);
    assert_eq!(lane.judge(channel, tmux, &uuid()), healthy);

    let (a, _) = launch_then_clear(&lane, channel, tmux, true);
    restart(channel);
    stamp(tmux);
    let legacy = PendingRestore::NotEligible(NotEligible::LegacyNonceNone);
    assert_eq!(lane.judge(channel, tmux, &a), RestoreStep::Finished(legacy));

    let (channel, tmux) = (7_511, "p2b-unmarked");
    stamp(tmux);
    let (a, _) = launch_then_clear(&lane, channel, tmux, true);
    restart(channel);
    fs::remove_file(tc::session_temp_path(tmux, "spawn_nonce")).unwrap();
    let unmarked = PendingRestore::NotEligible(NotEligible::NoNonceMarker);
    assert_eq!(
        lane.judge(channel, tmux, &a),
        RestoreStep::Finished(unmarked)
    );

    let launch = LaunchTranscript {
        session_id: a.clone(),
        transcript: lane.path(&a),
    };
    let unreadable = SpawnNonceMarker::Unreadable;
    let judged = |records| judge_restore(tmux, records, &unreadable, Some(&launch), |_| true);
    let down = |why| RestoreStep::Finished(PendingRestore::Unavailable(why));
    let log_error = judged(Err(io::Error::other("log read")));
    assert_eq!(log_error, down(Unavailable::LogRead(io::ErrorKind::Other)));
    let marker_error = judged(records_strict(channel));
    assert_eq!(marker_error, down(Unavailable::NonceMarkerUnreadable));
    for step in [log_error, marker_error] {
        let RestoreStep::Finished(outcome) = step else {
            unreachable!()
        };
        assert!(!outcome.memo(), "a read error is judged again");
    }
}

#[test]
fn a_pending_refused_before_the_restart_stays_refused_until_it_resolves() {
    let lane = Lane::new();
    let (channel, tmux) = (7_512, "p2b-refused");
    stamp(tmux);
    let (a, b, c) = (uuid(), uuid(), uuid());
    lane.touch(&a);
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &a, tmux);
    register_launched_tmux_runtime_binding(tmux, claude(&lane.path(&a), &a));
    let c_path = lane.touch(&c);
    let adopted = adopt_claude_continuation_session(&a, &c, &clear(&c_path));
    assert!(adopted.unwrap().is_some(), "C is bound");
    let pending = adopt_claude_continuation_session(&a, &b, &clear(&lane.path(&b)));
    assert!(pending.unwrap().is_none(), "B waits for its transcript");
    lane.touch(&b);
    let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    let c_file = fs::File::options().write(true).open(&c_path).unwrap();
    c_file.set_modified(newer).unwrap();
    let refused = adopt_claude_continuation_session(&a, &b, &clear(&lane.path(&b)));
    assert!(refused.unwrap().is_none(), "B is older than bound C");
    let records = records_strict(channel).unwrap().unwrap();
    let pending_seq = records[records.len() - 2].seq;
    assert!(matches!(
        records[records.len() - 2].new,
        BindingTarget::Pending { .. }
    ));
    assert!(matches!(
        records.last().unwrap().new,
        BindingTarget::Rejected { .. }
    ));
    restart(channel);
    let skip = PendingRestore::NotEligible(NotEligible::Rejected);
    assert_eq!(
        lane.judge(channel, tmux, &a),
        RestoreStep::Finished(skip),
        "a refused B is not seeded behind launch A"
    );

    let b_path = lane.path(&b);
    let rebound =
        register_rehydrated_tmux_runtime_binding("claude", tmux, channel, claude(&b_path, &b));
    assert!(rebound);
    let records = records_strict(channel).unwrap().unwrap();
    assert!(matches!(
        records.last().unwrap().new,
        BindingTarget::Resolved { pending_seq: seq, .. } if seq == pending_seq
    ));
    restart(channel);
    let exact = exact(lane.judge(channel, tmux, &a));
    assert_eq!(
        exact.session_id, b,
        "a Resolved after the refusal restores B"
    );

    // The restart forgot every binding; the pane is back on B, as a rehydrate leaves it.
    let bound = claude(&b_path, &b);
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude", tmux, channel, bound
    ));
    register_provider_session("claude", &a, tmux);
    let back_to_c = adopt_claude_continuation_session(&a, &c, &clear(&c_path));
    assert!(back_to_c.unwrap().is_some(), "newer C is bound again");
    fs::remove_file(&b_path).unwrap();
    let again = adopt_claude_continuation_session(&a, &b, &clear(&b_path));
    assert!(again.unwrap().is_none(), "B is a candidate again");
    lane.touch(&b);
    let refused = adopt_claude_continuation_session(&a, &b, &clear(&b_path));
    assert!(
        refused.unwrap().is_none(),
        "the second B is older than C too"
    );
    let records = records_strict(channel).unwrap().unwrap();
    let last = &records.last().unwrap().new;
    assert!(
        matches!(last, BindingTarget::Rejected { .. }),
        "second refusal on record: {last:?}"
    );
    restart(channel);
    let skip = PendingRestore::NotEligible(NotEligible::Rejected);
    assert_eq!(
        lane.judge(channel, tmux, &a),
        RestoreStep::Finished(skip),
        "a B refused again is not restored"
    );
}

#[test]
fn pending_path_away_from_the_launch_directory_is_blocked() {
    let lane = Lane::new();
    let (channel, tmux) = (7_512, "p2b-path");
    stamp(tmux);
    let (a, b) = (uuid(), uuid());
    lane.touch(&a);
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &a, tmux);
    register_launched_tmux_runtime_binding(tmux, claude(&lane.path(&a), &a));
    let elsewhere = lane.root.path().join(format!("{b}.jsonl"));
    assert!(
        adopt_claude_continuation_session(&a, &b, &clear(&elsewhere))
            .unwrap()
            .is_none()
    );
    restart(channel);

    let kind = CorruptKind::PathMismatch;
    let blocked = RestoreStep::Finished(PendingRestore::BlockedCorrupt(Corrupt { line: 2, kind }));
    assert_eq!(lane.judge(channel, tmux, &a), blocked);
}

#[test]
fn strict_read_takes_blank_and_crlf_lines_as_the_writer_does() {
    let lane = Lane::new();
    let (channel, tmux) = (7_513, "p2b-crlf");
    stamp(tmux);
    let (a, _) = launch_then_clear(&lane, channel, tmux, true);
    restart(channel);
    let expected = records_strict(channel).unwrap().unwrap();
    let text = fs::read_to_string(lane.log(channel)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let (first, second) = (lines[0], lines[1]);
    for (form, edited) in [
        ("blank", format!("{first}\n\n{second}\n")),
        ("crlf", format!("{first}\r\n{second}\r\n")),
    ] {
        fs::write(lane.log(channel), edited).unwrap();
        let read = records_strict(channel).unwrap().unwrap();
        assert_eq!(read, expected, "{form} lines read as the writer reads them");
        let step = lane.judge(channel, tmux, &a);
        assert!(
            matches!(step, RestoreStep::SeedAfterLaunch(_)),
            "{form}: {step:?}"
        );
    }
    fs::write(lane.log(channel), format!("{first}\n\r\n{second}\n")).unwrap();
    let kind = CorruptKind::Unparseable;
    let blocked = RestoreStep::Finished(PendingRestore::BlockedCorrupt(Corrupt { line: 2, kind }));
    assert_eq!(
        lane.judge(channel, tmux, &a),
        blocked,
        "a bare CR line blocks"
    );
}

#[test]
fn another_sessions_pending_does_not_repeat_an_earlier_refusal() {
    let lane = Lane::new();
    let (channel, tmux) = (7_514, "p2b-refused-once");
    stamp(tmux);
    let (a, b, c, d) = (uuid(), uuid(), uuid(), uuid());
    lane.touch(&a);
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &a, tmux);
    register_launched_tmux_runtime_binding(tmux, claude(&lane.path(&a), &a));
    let b_path = lane.touch(&b);
    let c_path = lane.touch(&c);
    let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    let c_file = fs::File::options().write(true).open(&c_path).unwrap();
    c_file.set_modified(newer).unwrap();
    assert!(
        adopt_claude_continuation_session(&a, &c, &clear(&c_path))
            .unwrap()
            .is_some()
    );
    let refuse_b = || adopt_claude_continuation_session(&a, &b, &clear(&b_path));
    assert!(refuse_b().unwrap().is_none(), "B is older than bound C");
    let pending = adopt_claude_continuation_session(&a, &d, &clear(&lane.path(&d)));
    assert!(pending.unwrap().is_none(), "D waits for its transcript");
    assert!(refuse_b().unwrap().is_none(), "B is refused again");
    let records = records_strict(channel).unwrap().unwrap();
    let refusals = records.iter().filter(|r| {
        matches!(&r.new, BindingTarget::Rejected { payload_session_id, .. } if *payload_session_id == b)
    });
    assert_eq!(
        refusals.count(),
        1,
        "another session's Pending repeats no refusal of B"
    );
}

/// A pane launched on an existing A, bound and logging to `channel`.
fn launched(lane: &Lane, channel: u64, tmux: &str) -> String {
    let a = uuid();
    lane.touch(&a);
    stamp(tmux);
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &a, tmux);
    register_launched_tmux_runtime_binding(tmux, claude(&lane.path(&a), &a));
    a
}

fn bound_session(tmux: &str) -> Option<String> {
    runtime_binding_for_tmux_session(tmux).and_then(|b| b.session_id)
}

#[test]
fn a_waiting_pending_is_replaced_by_the_next_clear_of_its_pane() {
    let lane = Lane::new();
    let (channel, tmux) = (7_515, "p2b-replace");
    let l = launched(&lane, channel, tmux);
    let (x, y) = (uuid(), uuid());
    let pending = AdoptionHttp::Durable(DurableKind::Pending);
    assert_eq!(adopt_from_hook(&l, &x, &clear(&lane.path(&x))), pending);
    let second = adopt_from_hook(&l, &y, &clear(&lane.path(&y)));
    assert_eq!(second, pending, "Y replaces the waiting X");
    assert_eq!(deferred_adoption_count(), 1);
    let y_seq = records_strict(channel)
        .unwrap()
        .unwrap()
        .last()
        .unwrap()
        .seq;

    lane.touch(&y);
    retry_deferred_claude_adoptions();
    retry_deferred_claude_adoptions();
    let records = records_strict(channel).unwrap().unwrap();
    let last = &records.last().unwrap().new;
    assert!(
        matches!(last, BindingTarget::Resolved { pending_seq, source } if *pending_seq == y_seq && source.session_id == y),
        "Y resolved: {last:?}"
    );
    assert_eq!(bound_session(tmux), Some(y));
    let adopted_x = records.iter().any(|r| match &r.new {
        BindingTarget::Source(s) | BindingTarget::Resolved { source: s, .. } => s.session_id == x,
        _ => false,
    });
    assert!(!adopted_x, "X is never adopted");
}

#[test]
fn a_waiting_pending_holds_only_its_own_pane_and_the_poll_returns() {
    let lane = Lane::new();
    let (ch1, p1, ch2, p2) = (7_516, "p2b-hold", 7_517, "p2b-hold-other");
    let a1 = launched(&lane, ch1, p1);
    let a2 = launched(&lane, ch2, p2);
    let (b1, c2) = (uuid(), uuid());
    let pending = AdoptionHttp::Durable(DurableKind::Pending);
    assert_eq!(adopt_from_hook(&a1, &b1, &clear(&lane.path(&b1))), pending);
    lane.touch(&c2);
    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    let refused = adopt_from_hook(&a2, &c2, &clear(&lane.path(&c2)));
    APPEND_FAULT.with(|fault| fault.set(None));
    assert_eq!(refused, AdoptionHttp::NotDurable(NotDurableReason::Append));
    assert_eq!(deferred_adoption_count(), 2, "B1 waits and C2 is queued");
    let lines = records_strict(ch1).unwrap().unwrap().len();

    // The poll runs on its own thread so one that never returns fails here instead of hanging.
    let root = lane.root.path().to_owned();
    let (done, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        set_test_root(Some(&root));
        retry_deferred_claude_adoptions();
        set_test_root(None);
        let _ = done.send(());
    });
    let poll = returned.recv_timeout(std::time::Duration::from_secs(2));
    poll.expect("poll returned");
    let waited = records_strict(ch1).unwrap().unwrap().len();
    assert_eq!(waited, lines, "the waiting Pending logs nothing more");
    assert_eq!(bound_session(p2), Some(c2), "the other pane is adopted");
    assert!(clear_claude_session_rotation(p2));
    retry_deferred_claude_adoptions();
    assert_eq!(deferred_adoption_count(), 1, "only B1 stays queued");

    lane.touch(&b1);
    retry_deferred_claude_adoptions();
    let records = records_strict(ch1).unwrap().unwrap();
    assert!(matches!(
        records.last().unwrap().new,
        BindingTarget::Resolved { .. }
    ));
    assert_eq!(bound_session(p1), Some(b1));
}
