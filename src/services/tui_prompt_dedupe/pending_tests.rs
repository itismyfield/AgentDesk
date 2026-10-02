use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::hook_server::adoption_retry::{
    AdoptionHttp, DurableKind, NotDurableReason, adopt_from_hook, deferred_adoption_count,
    reset_deferred_adoptions_for_tests,
};
use crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions;
use crate::services::claude_tui::source_verify::SourceRejection;
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::binding_context::{
    observe_spawn_nonce_marker, tests::fixture_after_shared_test_env_lock,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    APPEND_FAULT, BINDING_EVENTS_DIR, forget_channel_for_tests, pinned_source, record_pending,
    records_strict, set_test_root,
};
use crate::services::tui_prompt_dedupe::{
    AFTER_CHECK, AdoptSkip, TEST_LOCK, TuiRuntimeBinding, adopt_claude_continuation_explained,
    adopt_claude_continuation_session, clear_claude_session_rotation,
    lock_claude_session_rotations_for_tests, register_launched_tmux_runtime_binding,
    register_provider_session, register_rehydrated_tmux_runtime_binding, register_tmux_channel,
    reset_state_for_tests, resolve_tmux_session_name, runtime_binding_for_tmux_session,
};
use std::fs;
use std::sync::MutexGuard;

/// Real writers on a scratch log root and spawn-marker root, with the dedupe state serialised.
/// Fields drop in order: the env is restored while `TEST_LOCK` and then the env lock are held.
struct Lane {
    root: tempfile::TempDir,
    dir: tempfile::TempDir,
    _env: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
    _rotations: MutexGuard<'static, ()>,
    _state: MutexGuard<'static, ()>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Lane {
    fn new() -> Self {
        // Env lock, then `TEST_LOCK`, then the env change: a test holding `TEST_LOCK` never sees
        // the runtime root, and so the source-authority key, move under it.
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let state = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = fixture_after_shared_test_env_lock();
        let rotations = lock_claude_session_rotations_for_tests();
        let root = tempfile::tempdir().unwrap();
        set_test_root(Some(root.path()));
        restart(0);
        let dir = tempfile::tempdir().unwrap();
        Self {
            root,
            dir,
            _env: env,
            _rotations: rotations,
            _state: state,
            _env_lock: env_lock,
        }
    }

    fn path(&self, session: &str) -> PathBuf {
        self.dir.path().join(format!("{session}.jsonl"))
    }

    fn touch(&self, session: &str) -> PathBuf {
        let path = self.path(session);
        let row = serde_json::json!({"type": "mode", "sessionId": session});
        fs::write(&path, format!("{row}\n")).unwrap();
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
        let pinned = pinned_source(channel, tmux).unwrap();
        let records = records_strict(channel);
        judge_restore(
            tmux,
            records,
            pinned.as_ref(),
            &marker,
            Some(&launch),
            Path::is_file,
            observe_transcript,
        )
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

/// A /clear SessionStart whose relay publish time is `secs` past a fixed epoch.
fn clear_at(transcript: &Path, secs: i64) -> HookSignal {
    let published_at = chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0);
    HookSignal {
        published_at,
        ..clear(transcript)
    }
}

/// A prompt whose relay publish time is `secs` past the same epoch as `clear_at`.
fn prompt_at(transcript: &Path, secs: i64) -> HookSignal {
    let payload = serde_json::json!({ "transcript_path": transcript });
    HookSignal {
        published_at: chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0),
        ..HookSignal::from_payload("user_prompt_submit", &payload)
    }
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

/// The restore a rehydrate pass runs for the pane, registering what it judged.
fn restore(channel: u64, tmux: &str, launch: &str, launch_path: &Path) -> PendingRestore {
    let launch = LaunchTranscript {
        session_id: launch.to_owned(),
        transcript: launch_path.to_path_buf(),
    };
    let bind = |session: &str, path: &Path| claude(path, session);
    restore_claude_pane(tmux, channel, Some(launch), bind).expect("the pane is judged")
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
    persisted: Some(Persisted::Logged),
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

    // Pending A is not restored; the verified Source B that superseded it is.
    let restored = exact(lane.judge(channel, tmux, &a));
    assert_eq!(
        (restored.session_id, restored.transcript),
        (b, b_path),
        "Pending A superseded by Source B"
    );
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
            persisted: binding.then_some(Persisted::Logged),
        }
    };

    // Judging loaded the writer; the fault needs it read from disk again.
    forget_channel_for_tests(channel);
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
    let gone = lane.path(&b);
    let seam = move || fs::remove_file(gone).unwrap();
    AFTER_CHECK.with_borrow_mut(|after| *after = Some(Box::new(seam)));
    let wait = ExactPathWait {
        session_id: b.clone(),
        transcript: lane.path(&b),
    };
    let expected = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: Some(wait),
    };
    assert_eq!(
        restore(channel, tmux, &a, &lane.path(&a)),
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
    let judged = |records| {
        judge_restore(
            tmux,
            records,
            None,
            &unreadable,
            Some(&launch),
            |_| true,
            observe_transcript,
        )
    };
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
    let pending = adopt_claude_continuation_session(&a, &b, &clear_at(&lane.path(&b), 10));
    assert!(pending.unwrap().is_none(), "B waits for its transcript");
    let c_path = lane.touch(&c);
    let adopted = adopt_claude_continuation_session(&a, &c, &prompt_at(&c_path, 20));
    assert!(
        adopted.unwrap().is_some(),
        "C is bound and supersedes the waiting B"
    );
    lane.touch(&b);
    let refused = adopt_claude_continuation_session(&a, &b, &clear_at(&lane.path(&b), 15));
    assert!(
        refused.unwrap().is_none(),
        "B's hook predates the pane leaving B"
    );
    let records = records_strict(channel).unwrap().unwrap();
    let kinds = |r: &BindingEvent| match &r.new {
        BindingTarget::Pending { .. } => "pending",
        BindingTarget::Source(_) => "source",
        BindingTarget::Resolved { .. } => "resolved",
        BindingTarget::Rejected { .. } => "rejected",
    };
    let tail: Vec<_> = records[records.len() - 3..].iter().map(kinds).collect();
    assert_eq!(tail, ["pending", "source", "rejected"]);
    restart(channel);
    // B is not restored; the pane gets back the verified C it stayed on.
    let restored = exact(lane.judge(channel, tmux, &a));
    assert_eq!(
        restored.session_id, c,
        "a refused B is not seeded behind launch A"
    );

    let b_path = lane.path(&b);
    let rebound =
        register_rehydrated_tmux_runtime_binding("claude", tmux, channel, claude(&b_path, &b));
    assert!(rebound);
    let records = records_strict(channel).unwrap().unwrap();
    // The superseded Pending is never resolved; the registration logs B as a source of its own.
    assert_eq!(kinds(records.last().unwrap()), "source");
    restart(channel);
    // An unverified B is not the pin, so the restore leaves the pane to the rehydrate pass.
    let step = format!("{:?}", lane.judge(channel, tmux, &a));
    assert_eq!(step, "Finished(NotEligible(Superseded))");

    // The restart forgot every binding; the pane is back on B, as a rehydrate leaves it.
    let bound = claude(&b_path, &b);
    assert!(register_rehydrated_tmux_runtime_binding(
        "claude", tmux, channel, bound
    ));
    register_provider_session("claude", &a, tmux);
    let back_to_c = adopt_claude_continuation_session(&a, &c, &clear_at(&c_path, 30));
    assert!(back_to_c.unwrap().is_some(), "C is bound again");
    let refused = adopt_claude_continuation_session(&a, &b, &clear_at(&b_path, 25));
    assert!(
        refused.unwrap().is_none(),
        "B's second hook predates the pane leaving B too"
    );
    let records = records_strict(channel).unwrap().unwrap();
    let refusals = records.iter().filter(|r| {
        matches!(&r.new, BindingTarget::Rejected { payload_session_id, reason, .. }
            if *payload_session_id == b && reason == "regression")
    });
    assert_eq!(refusals.count(), 1, "the same judgment of B is logged once");
    restart(channel);
    let restored = exact(lane.judge(channel, tmux, &a));
    assert_eq!(restored.session_id, c, "a B refused again is not restored");
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
    // One directory deeper than `<projects root>/<project>/<id>.jsonl`.
    let elsewhere = lane.root.path().join("nested").join(format!("{b}.jsonl"));
    let refused = adopt_claude_continuation_explained(&a, &b, &clear(&elsewhere)).unwrap();
    let not_top_level = AdoptSkip::SourceRejected(SourceRejection::NotTopLevelTranscript);
    assert_eq!(refused, (None, Some(not_top_level)));
    // A Pending an older build logged for such a path is not restored either.
    let (hook, path) = (clear(&elsewhere), elsewhere.display().to_string());
    let proposal = Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: tmux,
        session_id: Some(&b),
        path: &path,
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Clear),
        hook: Some(&hook),
    };
    record_pending(&proposal).unwrap();
    restart(channel);

    let kind = CorruptKind::PathMismatch;
    let blocked = RestoreStep::Finished(PendingRestore::BlockedCorrupt(Corrupt { line: 3, kind }));
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
    assert!(
        adopt_claude_continuation_session(&a, &c, &clear_at(&c_path, 20))
            .unwrap()
            .is_some()
    );
    let refuse_b = || adopt_claude_continuation_session(&a, &b, &clear_at(&b_path, 10));
    assert!(refuse_b().unwrap().is_none(), "B predates the switch to C");
    let pending = adopt_claude_continuation_session(&a, &d, &clear_at(&lane.path(&d), 30));
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

#[test]
fn a_late_hook_refused_as_left_leaves_the_waiting_pending_queued() {
    let lane = Lane::new();
    let (channel, tmux) = (7_518, "p2b-late-hook");
    let a = launched(&lane, channel, tmux);
    let (b, c, d) = (uuid(), uuid(), uuid());
    lane.touch(&c);
    let b_path = lane.touch(&b);
    let adopted = AdoptionHttp::Durable(DurableKind::Adopted);
    assert_eq!(adopt_from_hook(&a, &b, &clear_at(&b_path, 10)), adopted);
    assert_eq!(
        adopt_from_hook(&a, &c, &prompt_at(&lane.path(&c), 20)),
        adopted
    );
    assert!(clear_claude_session_rotation(tmux));
    let pending = AdoptionHttp::Durable(DurableKind::Pending);
    assert_eq!(
        adopt_from_hook(&a, &d, &clear_at(&lane.path(&d), 30)),
        pending
    );

    // B's own start, delivered only now, was published before the pane left B.
    let late = adopt_from_hook(&a, &b, &clear_at(&b_path, 15));
    let regression = AdoptSkip::SourceRejected(SourceRejection::Regression);
    assert_eq!(late, AdoptionHttp::Skipped(regression));
    assert_eq!(deferred_adoption_count(), 1, "D stays queued");
    lane.touch(&d);
    retry_deferred_claude_adoptions();
    assert_eq!(bound_session(tmux), Some(d));
}

/// Continuation adoption follows the payload's own transcript once the Claude source check passes.
/// Transcripts live under `<home>/projects/<project>/<session>.jsonl`, as Claude writes them.
mod verified_adoption {
    use super::*;
    use crate::services::claude_tui::hook_server::observation_ingress::{
        IngressOutcome, UnavailableReason, observe_binding_hook,
    };
    use crate::services::tui_o::shadow::capture::file_identity;
    use crate::services::tui_prompt_dedupe::binding_events::{
        SourceId, binding_events_since, record_verified,
    };
    use crate::services::tui_prompt_dedupe::{
        claude_session_rotation_for_tmux, forget_hook_adopted_claude_session_id,
        hook_adopted_claude_session_id, register_tmux_runtime_binding,
    };

    fn first_row(session: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "mode", "sessionId": session})
        )
    }

    /// `<home>/projects/<project>/<session>.jsonl`, written with `body` when it is given.
    fn at(home: &Path, project: &str, session: &str, body: Option<&str>) -> PathBuf {
        let path = home
            .join("projects")
            .join(project)
            .join(format!("{session}.jsonl"));
        if let Some(body) = body {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
        }
        path
    }

    fn signal(event: &str, source: Option<&str>, transcript: Option<&Path>) -> HookSignal {
        let payload = serde_json::json!({ "source": source, "transcript_path": transcript });
        HookSignal::from_payload(event, &payload)
    }

    fn stop(transcript: &Path) -> HookSignal {
        signal("stop", None, Some(transcript))
    }

    /// A launched pane bound to `a_path`, logging to `channel` under a fresh spawn nonce.
    fn pane(channel: u64, tmux: &str, a: &str, a_path: &Path) {
        stamp(tmux);
        register_tmux_channel(tmux, channel);
        register_provider_session("claude", a, tmux);
        register_launched_tmux_runtime_binding(tmux, claude(a_path, a));
    }

    fn adopt(a: &str, b: &str, hook: &HookSignal) -> (Option<(String, String)>, Option<AdoptSkip>) {
        adopt_claude_continuation_explained(a, b, hook).expect("binding event persisted")
    }

    fn log(channel: u64) -> Vec<BindingEvent> {
        binding_events_since(channel, 0).unwrap()
    }

    fn last(channel: u64) -> BindingTarget {
        log(channel).pop().unwrap().new
    }

    fn bound(tmux: &str) -> TuiRuntimeBinding {
        runtime_binding_for_tmux_session(tmux).unwrap()
    }

    /// Whether the log pins the pane's current binding to a file identity.
    fn pinned(channel: u64, tmux: &str) -> bool {
        let binding = bound(tmux);
        let session = binding.session_id.unwrap_or_default();
        let pin = pinned_source(channel, tmux).unwrap();
        pin.is_some_and(|pin| {
            pin.session_id == session && pin.path == Path::new(&binding.output_path)
        })
    }

    fn source(session: &str, path: &Path) -> SourceId {
        let (dev, ino) = file_identity(&fs::metadata(path).unwrap());
        let (session_id, path) = (session.to_owned(), path.to_path_buf());
        SourceId {
            session_id,
            path,
            dev,
            ino,
        }
    }

    fn rejected(channel: u64) -> Option<String> {
        match last(channel) {
            BindingTarget::Rejected { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// Writes a new file with `body` and renames it over `path`, so the path names another inode.
    fn replace(path: &Path, body: &str) {
        let replacement = path.with_extension("tmp");
        fs::write(&replacement, body).unwrap();
        fs::rename(&replacement, path).unwrap();
    }

    #[test]
    fn a_resume_into_another_worktree_follows_the_payload_transcript() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, y) = (7_600, "n2a-worktree", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        // `/resume` in the TUI picked a session of another worktree's project directory.
        let y_path = at(home.path(), "-work-b", &y, Some(&first_row(&y)));
        let resume = signal("session_start", Some("resume"), Some(&y_path));

        let (adopted, skip) = adopt(&a, &y, &resume);

        let y_text = y_path.display().to_string();
        assert_eq!(
            (adopted, skip),
            (Some((tmux.to_owned(), y_text.clone())), None)
        );
        assert_eq!(bound(tmux).output_path, y_text);
        assert_eq!(last(channel), BindingTarget::Source(source(&y, &y_path)));
        assert!(pinned(channel, tmux), "the adopted file is pinned");
        let rotation = claude_session_rotation_for_tmux(tmux).expect("rotation queued");
        assert_eq!(rotation.old_session_id.as_deref(), Some(a.as_str()));
    }

    #[test]
    fn a_payload_transcript_that_fails_the_check_is_refused_and_audited() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let other_home = tempfile::tempdir().unwrap();
        for (index, case) in ["subagent", "foreign first record", "other root", "no path"]
            .into_iter()
            .enumerate()
        {
            let (channel, tmux) = (7_610 + index as u64, format!("n2a-refused-{index}"));
            let (a, b) = (uuid(), uuid());
            let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
            pane(channel, &tmux, &a, &a_path);
            // Next to A so the pre-check `parent + id` rule would have adopted it.
            let sibling = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
            let (payload, want, reason) = match case {
                "subagent" => {
                    let child = a_path.with_extension("").join("subagents/agent-x.jsonl");
                    fs::create_dir_all(child.parent().unwrap()).unwrap();
                    fs::write(&child, first_row(&b)).unwrap();
                    let why = SourceRejection::NotTopLevelTranscript;
                    (
                        Some(child),
                        AdoptSkip::SourceRejected(why),
                        Some("not_top_level_transcript"),
                    )
                }
                "foreign first record" => {
                    fs::write(&sibling, first_row(&uuid())).unwrap();
                    let why = SourceRejection::FirstRecordMismatch;
                    (
                        Some(sibling),
                        AdoptSkip::SourceRejected(why),
                        Some("first_record_mismatch"),
                    )
                }
                "other root" => {
                    let elsewhere = at(other_home.path(), "-work-a", &b, Some(&first_row(&b)));
                    let why = SourceRejection::NotTopLevelTranscript;
                    (
                        Some(elsewhere),
                        AdoptSkip::SourceRejected(why),
                        Some("not_top_level_transcript"),
                    )
                }
                _ => (None, AdoptSkip::PayloadPathMissing, None),
            };
            let records = log(channel).len();

            let result = adopt(&a, &b, &signal("stop", None, payload.as_deref()));

            assert_eq!(result, (None, Some(want)), "{case}");
            assert_eq!(
                bound(&tmux).output_path,
                a_path.display().to_string(),
                "{case}"
            );
            assert_eq!(
                log(channel).len(),
                records + usize::from(reason.is_some()),
                "{case}"
            );
            assert_eq!(rejected(channel).as_deref(), reason, "{case}");
        }
    }

    #[test]
    fn a_transcript_before_its_first_record_is_pending_until_it_is_verified() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_620, "n2a-not-written", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        // Claude has created B but not finished its first line.
        let row = first_row(&b);
        let b_path = at(home.path(), "-work-a", &b, Some(row.trim_end()));
        let clear = signal("session_start", Some("clear"), Some(&b_path));
        APPEND_FAULT.with(|fault| fault.set(Some("sync")));
        assert!(adopt_claude_continuation_explained(&a, &b, &clear).is_err());
        APPEND_FAULT.with(|fault| fault.set(None));

        // The retried hook finds the file but still no first record: B waits as a Pending.
        assert_eq!(adopt(&a, &b, &clear), (None, None));
        let pending_seq = log(channel).pop().unwrap().seq;
        let b_text = Some(b_path.display().to_string());
        let pending = BindingTarget::Pending {
            payload_session_id: b.clone(),
            payload_transcript_path: b_text,
        };
        assert_eq!(last(channel), pending);
        assert_eq!(bound(tmux).output_path, a_path.display().to_string());

        fs::write(&b_path, &row).unwrap();
        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        let source = source(&b, &b_path);
        assert_eq!(
            last(channel),
            BindingTarget::Resolved {
                pending_seq,
                source
            }
        );
        assert!(pinned(channel, tmux));
    }

    #[test]
    fn a_replaced_bound_transcript_stops_instead_of_rebinding() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_630, "n2a-replaced", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        assert!(pinned(channel, tmux));
        let before = bound(tmux);

        replace(&b_path, &first_row(&b));
        assert!(forget_hook_adopted_claude_session_id(tmux));
        let result = adopt(&a, &b, &stop(&b_path));

        assert_eq!(result, (None, Some(AdoptSkip::SourceAnomaly)));
        assert_eq!(rejected(channel).as_deref(), Some("source_anomaly"));
        assert_eq!(
            hook_adopted_claude_session_id(tmux),
            None,
            "the hook restates nothing"
        );
        assert_eq!(bound(tmux), before);
    }

    #[test]
    fn an_unpinned_current_source_is_pinned_once_and_the_pin_survives_a_restart() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_640, "n2a-pin-upgrade", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        // A registration without a check, as a record from before the check existed.
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        let registered = TuiRuntimeBinding {
            last_offset: 12,
            ..claude(&b_path, &b)
        };
        register_tmux_runtime_binding(tmux, registered.clone());
        assert_eq!(last(channel), BindingTarget::Source(source(&b, &b_path)));
        assert!(!pinned(channel, tmux));

        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        let pin = log(channel).pop().unwrap();
        assert_eq!(pin.new, BindingTarget::Source(source(&b, &b_path)));
        assert_eq!(pin.parent_hint, None);
        assert!(pinned(channel, tmux));
        assert!(
            claude_session_rotation_for_tmux(tmux).is_none(),
            "no rotation"
        );
        assert_eq!(bound(tmux), registered, "the cursor stays where it was");
        let records = log(channel).len();
        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        assert_eq!(
            log(channel).len(),
            records,
            "a pinned source is not logged again"
        );

        restart(channel);
        register_tmux_channel(tmux, channel);
        register_provider_session("claude", &a, tmux);
        register_rehydrated_tmux_runtime_binding("claude", tmux, channel, registered);
        replace(&b_path, &first_row(&b));
        let result = adopt(&a, &b, &stop(&b_path));
        assert_eq!(result, (None, Some(AdoptSkip::SourceAnomaly)));
    }

    #[test]
    fn a_verified_record_keeps_the_identity_the_check_read() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, b) = (7_650, "n2a-writer", uuid());
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        register_tmux_channel(tmux, channel);
        // The identity the check read, which the path no longer names.
        let checked = SourceId {
            ino: source(&b, &b_path).ino + 1,
            ..source(&b, &b_path)
        };
        let (path, hook) = (b_path.display().to_string(), stop(&b_path));
        let proposal = Proposal {
            channel_id: channel,
            provider: "claude",
            tmux_session: tmux,
            session_id: Some(&b),
            path: &path,
            replaced: None,
            cause: CauseSource::Hook(BindingCause::Unknown),
            hook: Some(&hook),
        };

        record_verified(&proposal, &checked).unwrap();

        assert_eq!(last(channel), BindingTarget::Source(checked));
    }

    #[test]
    fn an_unreadable_new_transcript_is_kept_as_a_durable_pending() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_660, "n2a-unreadable", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        // A path that opens but cannot be read as a file.
        let b_path = at(home.path(), "-work-a", &b, None);
        fs::create_dir_all(&b_path).unwrap();
        let clear = signal("session_start", Some("clear"), Some(&b_path));

        let pending = AdoptionHttp::Durable(DurableKind::Pending);
        assert_eq!(adopt_from_hook(&a, &b, &clear), pending);
        let pending_seq = log(channel).pop().unwrap().seq;
        assert!(
            matches!(last(channel), BindingTarget::Pending { payload_session_id, .. } if payload_session_id == b)
        );
        assert_eq!(deferred_adoption_count(), 1);

        fs::remove_dir(&b_path).unwrap();
        fs::write(&b_path, first_row(&b)).unwrap();
        retry_deferred_claude_adoptions();
        assert_eq!(bound(tmux).output_path, b_path.display().to_string());
        let source = source(&b, &b_path);
        assert_eq!(
            last(channel),
            BindingTarget::Resolved {
                pending_seq,
                source
            }
        );
    }

    #[test]
    fn an_unreadable_binding_log_refuses_the_hook_until_it_loads() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_670, "n2a-log-down", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        let payload = serde_json::json!({ "session_id": b, "transcript_path": b_path });
        let observe = || {
            let headers = axum::http::HeaderMap::new();
            observe_binding_hook("claude", "stop", Some(&a), Some(&b), &payload, &headers)
        };
        forget_channel_for_tests(channel);
        APPEND_FAULT.with(|fault| fault.set(Some("reload")));

        let refused = IngressOutcome::Unavailable(UnavailableReason::HistoryUnreadable);
        assert_eq!(observe(), refused);
        assert_eq!(deferred_adoption_count(), 0, "the sender retries it");
        assert_eq!(bound(tmux).output_path, a_path.display().to_string());

        APPEND_FAULT.with(|fault| fault.set(None));
        assert_eq!(observe(), IngressOutcome::Durable(DurableKind::Adopted));
        assert_eq!(bound(tmux).output_path, b_path.display().to_string());
    }

    #[test]
    fn a_front_held_by_an_unreadable_log_keeps_its_pending_mark() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b, e) = (7_680, "n2a-transient-front", uuid(), uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let pending = AdoptionHttp::Durable(DurableKind::Pending);
        let b_clear = signal(
            "session_start",
            Some("clear"),
            Some(&at(home.path(), "-work-a", &b, None)),
        );
        assert_eq!(adopt_from_hook(&a, &b, &b_clear), pending);
        forget_channel_for_tests(channel);
        APPEND_FAULT.with(|fault| fault.set(Some("reload")));
        retry_deferred_claude_adoptions();
        APPEND_FAULT.with(|fault| fault.set(None));
        assert_eq!(deferred_adoption_count(), 1, "B is held, not dropped");

        // B is still a Pending waiting for its file, so the next clear replaces it.
        let e_clear = signal(
            "session_start",
            Some("clear"),
            Some(&at(home.path(), "-work-a", &e, None)),
        );
        assert_eq!(adopt_from_hook(&a, &e, &e_clear), pending);
        assert_eq!(deferred_adoption_count(), 1);
        assert!(
            matches!(last(channel), BindingTarget::Pending { payload_session_id, .. } if payload_session_id == e)
        );
    }

    #[test]
    fn a_payload_spelled_through_a_symlinked_root_is_adopted_under_the_bound_spelling() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let real = home.path().join("real");
        let link = home.path().join("link");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (channel, tmux, a, b) = (7_690, "n2a-symlink", uuid(), uuid());
        at(&real, "-work-a", &a, Some(&first_row(&a)));
        let a_path = at(&link, "-work-a", &a, None);
        pane(channel, tmux, &a, &a_path);
        let b_real = at(&real, "-work-a", &b, Some(&first_row(&b)));
        let b_link = at(&link, "-work-a", &b, None).display().to_string();

        assert_eq!(
            adopt(&a, &b, &stop(&b_real)).0,
            Some((tmux.to_owned(), b_link.clone()))
        );
        assert_eq!(bound(tmux).output_path, b_link);
        let records = log(channel).len();
        assert!(adopt(&a, &b, &stop(&b_real)).0.is_some());
        assert_eq!(
            log(channel).len(),
            records,
            "the same file under either spelling"
        );
    }

    #[test]
    fn a_transcript_replaced_between_its_check_and_its_record_waits_for_the_next_check() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_700, "n2a-recheck", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        let checked = source(&b, &b_path);
        let (seam_path, row) = (b_path.clone(), first_row(&b));
        AFTER_CHECK
            .with_borrow_mut(|seam| *seam = Some(Box::new(move || replace(&seam_path, &row))));
        let clear = signal("session_start", Some("clear"), Some(&b_path));

        let pending = AdoptionHttp::Durable(DurableKind::Pending);
        assert_eq!(adopt_from_hook(&a, &b, &clear), pending);
        assert!(matches!(last(channel), BindingTarget::Pending { .. }));
        assert_eq!(bound(tmux).output_path, a_path.display().to_string());

        retry_deferred_claude_adoptions();
        let replaced = source(&b, &b_path);
        assert_ne!(replaced, checked);
        assert!(
            matches!(last(channel), BindingTarget::Resolved { source, .. } if source == replaced)
        );
        assert_eq!(bound(tmux).output_path, b_path.display().to_string());
    }

    fn judge_at(channel: u64, tmux: &str, a: &str, a_path: &Path) -> RestoreStep {
        let launch = LaunchTranscript {
            session_id: a.to_owned(),
            transcript: a_path.to_path_buf(),
        };
        let (records, marker) = (records_strict(channel), observe_spawn_nonce_marker(tmux));
        let pinned = pinned_source(channel, tmux).unwrap();
        judge_restore(
            tmux,
            records,
            pinned.as_ref(),
            &marker,
            Some(&launch),
            Path::is_file,
            observe_transcript,
        )
    }

    #[test]
    fn a_restored_pending_in_another_project_dir_is_seeded() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_710, "n2a-restore-other-dir", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-b", &b, None);
        let clear = signal("session_start", Some("clear"), Some(&b_path));
        assert_eq!(adopt(&a, &b, &clear), (None, None));
        restart(channel);

        let seed = seed(judge_at(channel, tmux, &a, &a_path));
        assert_eq!(seed.payload_session_id, b);
        assert_eq!(
            seed.hook.transcript_path,
            Some(b_path.display().to_string())
        );
    }

    #[test]
    fn a_restored_pending_whose_file_names_another_session_is_blocked() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_720, "n2a-restore-foreign", uuid(), uuid());
        // Launch A is known but its transcript is not written, so the restore publishes B itself.
        let a_path = at(home.path(), "-work-a", &a, None);
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, None);
        let clear = signal("session_start", Some("clear"), Some(&b_path));
        assert_eq!(adopt(&a, &b, &clear), (None, None));
        let pending_seq = log(channel).pop().unwrap().seq;
        at(home.path(), "-work-a", &b, Some(&first_row(&uuid())));
        restart(channel);

        let kind = CorruptKind::PathMismatch;
        let corrupt = Corrupt {
            line: pending_seq,
            kind,
        };
        let blocked = RestoreStep::Finished(PendingRestore::BlockedCorrupt(corrupt));
        assert_eq!(judge_at(channel, tmux, &a, &a_path), blocked);
    }

    #[test]
    fn a_verified_source_behind_a_refused_pending_is_restored_after_a_restart() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b, c) = (7_725, "n2a-restore-current", uuid(), uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-b", &b, Some(&first_row(&b)));
        let resume = signal("session_start", Some("resume"), Some(&b_path));
        assert!(adopt(&a, &b, &resume).0.is_some());
        // C waits as a Pending, then a later hook of C is refused, which leaves B current.
        let c_path = at(home.path(), "-work-a", &c, None);
        let clear = signal("session_start", Some("clear"), Some(&c_path));
        assert_eq!(adopt(&a, &c, &clear), (None, None));
        let nested = at(&home.path().join("projects/-work-a"), "sub", &c, None);
        let refused = adopt(&a, &c, &stop(&nested)).1;
        assert!(
            matches!(refused, Some(AdoptSkip::SourceRejected(_))),
            "{refused:?}"
        );
        restart(channel);

        let restored = exact(judge_at(
            channel,
            tmux,
            &b,
            &a_path.with_file_name(format!("{b}.jsonl")),
        ));
        assert_eq!((restored.session_id, restored.transcript), (b, b_path));
    }

    #[test]
    fn a_pinned_resolved_replaced_before_the_restart_is_an_anomaly_and_keeps_its_pin() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_730, "n2a-restore-pinned", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, None);
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, None);
        let clear = signal("session_start", Some("clear"), Some(&b_path));
        assert_eq!(adopt(&a, &b, &clear), (None, None));
        at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        let (pin, resolved) = (source(&b, &b_path), log(channel).pop().unwrap());
        assert!(matches!(&resolved.new, BindingTarget::Resolved { source, .. } if *source == pin));

        replace(&b_path, &first_row(&b));
        restart(channel);
        let logged = log(channel).len();
        let line = resolved.seq;
        assert_eq!(
            restore(channel, tmux, &a, &a_path),
            PendingRestore::Anomaly { line }
        );
        assert_eq!(log(channel).len(), logged, "no new identity logged");
        forget_channel_for_tests(channel);
        assert_eq!(pinned_source(channel, tmux).unwrap(), Some(pin));
        assert!(runtime_binding_for_tmux_session(tmux).is_none());
    }

    #[test]
    fn a_restored_transcript_replaced_before_its_record_is_checked_again_next_poll() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_740, "n2a-restore-recheck", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, None);
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, None);
        let clear = signal("session_start", Some("clear"), Some(&b_path));
        assert_eq!(adopt(&a, &b, &clear), (None, None));
        let pending_seq = log(channel).pop().unwrap().seq;
        at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        restart(channel);
        let (seam_path, row) = (b_path.clone(), first_row(&b));
        let seam = move || replace(&seam_path, &row);
        AFTER_CHECK.with_borrow_mut(|after| *after = Some(Box::new(seam)));
        let logged = log(channel).len();

        let waiting = restore(channel, tmux, &a, &a_path);
        let wait = ExactPathWait {
            session_id: b.clone(),
            transcript: b_path.clone(),
        };
        let exact_wait = Some(wait);
        let expected = PendingRestore::BoundFromLedger {
            pending_seq,
            exact_wait,
        };
        assert_eq!(waiting, expected);
        assert!(!waiting.memo(), "the next poll judges the pane again");
        assert_eq!(
            log(channel).len(),
            logged,
            "the replaced file is not logged"
        );
        assert_eq!(bound(tmux).output_path, b_path.display().to_string());

        let exact_wait = None;
        let settled = PendingRestore::BoundFromLedger {
            pending_seq,
            exact_wait,
        };
        assert_eq!(restore(channel, tmux, &a, &a_path), settled);
        let source = source(&b, &b_path);
        let resolved = BindingTarget::Resolved {
            pending_seq,
            source,
        };
        assert_eq!(last(channel), resolved);
    }

    #[test]
    fn a_current_source_without_its_first_record_is_logged_as_a_pending() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_750, "n2a-current-pending", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        // A registration without a check, of a transcript whose first line is not complete yet.
        let b_path = at(home.path(), "-work-a", &b, Some("{\"sessionId\":"));
        register_tmux_runtime_binding(tmux, claude(&b_path, &b));
        assert_eq!(last(channel), BindingTarget::Source(source(&b, &b_path)));
        let before = bound(tmux);

        let pending = AdoptionHttp::Durable(DurableKind::Pending);
        assert_eq!(adopt_from_hook(&a, &b, &stop(&b_path)), pending);
        forget_channel_for_tests(channel);
        let expected = BindingTarget::Pending {
            payload_session_id: b.clone(),
            payload_transcript_path: Some(b_path.display().to_string()),
        };
        let on_disk = records_strict(channel).unwrap().unwrap();
        assert_eq!(on_disk.last().unwrap().new, expected);
        assert_eq!(bound(tmux), before);
        assert_eq!(adopt(&a, &b, &stop(&b_path)), (None, None));
        assert_eq!(log(channel).len(), on_disk.len(), "one Pending, not two");
    }

    #[test]
    fn a_current_source_replaced_after_its_check_waits_as_a_pending() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_760, "n2a-confirm-recheck", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        let registered = TuiRuntimeBinding {
            last_offset: 12,
            ..claude(&b_path, &b)
        };
        register_tmux_runtime_binding(tmux, registered.clone());
        let (seam_path, row) = (b_path.clone(), first_row(&b));
        let seam = move || replace(&seam_path, &row);
        AFTER_CHECK.with_borrow_mut(|after| *after = Some(Box::new(seam)));

        let pending = AdoptionHttp::Durable(DurableKind::Pending);
        assert_eq!(adopt_from_hook(&a, &b, &stop(&b_path)), pending);
        assert!(
            matches!(last(channel), BindingTarget::Pending { payload_session_id, .. } if payload_session_id == b)
        );
        assert_eq!(bound(tmux), registered, "binding and cursor kept");

        retry_deferred_claude_adoptions();
        let replaced = source(&b, &b_path);
        assert!(
            matches!(last(channel), BindingTarget::Resolved { source, .. } if source == replaced)
        );
        assert!(pinned(channel, tmux));
        assert_eq!(bound(tmux), registered);
    }

    /// Makes `path` unopenable, or openable again, while a stat of it still succeeds.
    fn readable(path: &Path, readable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if readable { 0o644 } else { 0o000 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_pinned_current_behind_a_later_pending_is_judged_by_its_pin_before_the_seed() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b, c) = (7_770, "n2a-pin-behind-pending", uuid(), uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, None);
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, None);
        let clear = signal("session_start", Some("clear"), Some(&b_path));
        assert_eq!(adopt(&a, &b, &clear), (None, None));
        at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        let (pin, resolved) = (source(&b, &b_path), log(channel).pop().unwrap());
        // C waits as a Pending behind B; the launch selector now names B.
        let c_path = at(home.path(), "-work-a", &c, None);
        let c_clear = signal("session_start", Some("clear"), Some(&c_path));
        assert_eq!(adopt(&a, &c, &c_clear), (None, None));
        let pending_seq = log(channel).pop().unwrap().seq;
        let logged = log(channel).len();

        // Unreadable, the pinned B is neither bound nor logged, and the next poll judges it again.
        readable(&b_path, false);
        restart(channel);
        let unreadable = restore(channel, tmux, &b, &b_path);
        readable(&b_path, true);
        let down = PendingRestore::Unavailable(Unavailable::TranscriptUnreadable);
        assert_eq!(unreadable, down);
        assert!(!unreadable.memo());
        assert!(runtime_binding_for_tmux_session(tmux).is_none());

        // Its own file binds B, and C is seeded behind it; nothing is logged again.
        restart(channel);
        let seeded = restore(channel, tmux, &b, &b_path);
        assert_eq!(seeded, PendingRestore::Seeded { pending_seq });
        assert_eq!(bound(tmux).output_path, b_path.display().to_string());
        assert_eq!(deferred_adoption_count(), 1);
        assert_eq!(log(channel).len(), logged);

        // Replaced, it stays pinned to I1 and nothing binds over it.
        replace(&b_path, &first_row(&b));
        restart(channel);
        let line = resolved.seq;
        assert_eq!(
            restore(channel, tmux, &b, &b_path),
            PendingRestore::Anomaly { line }
        );
        assert_eq!(log(channel).len(), logged, "no new identity logged");
        forget_channel_for_tests(channel);
        assert_eq!(pinned_source(channel, tmux).unwrap(), Some(pin));
        assert!(runtime_binding_for_tmux_session(tmux).is_none());
        assert_eq!(deferred_adoption_count(), 0, "nothing is seeded");
    }

    #[test]
    fn a_stat_registration_of_a_replaced_pinned_transcript_is_refused() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_780, "n2a-stat-pin", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        let pin = source(&b, &b_path);
        let logged = log(channel).len();

        replace(&b_path, &first_row(&b));
        restart(channel);
        let registered = claude(&b_path, &b);
        assert!(!register_rehydrated_tmux_runtime_binding(
            "claude", tmux, channel, registered
        ));
        assert_eq!(
            log(channel).len(),
            logged,
            "the replaced file is not logged"
        );
        forget_channel_for_tests(channel);
        assert_eq!(pinned_source(channel, tmux).unwrap(), Some(pin));
        assert!(runtime_binding_for_tmux_session(tmux).is_none());
    }

    #[test]
    fn an_unreadable_unpinned_current_waits_as_a_durable_pending() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_790, "n2a-unpinned-unreadable", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        // A registration without a check, as a record from before the check existed.
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        register_tmux_runtime_binding(tmux, claude(&b_path, &b));
        assert!(!pinned(channel, tmux));
        let before = bound(tmux);

        readable(&b_path, false);
        let pending = AdoptionHttp::Durable(DurableKind::Pending);
        let answered = adopt_from_hook(&a, &b, &stop(&b_path));
        readable(&b_path, true);
        assert_eq!(answered, pending);
        forget_channel_for_tests(channel);
        let on_disk = records_strict(channel).unwrap().unwrap();
        let waiting = on_disk.last().unwrap();
        let expected = BindingTarget::Pending {
            payload_session_id: b.clone(),
            payload_transcript_path: Some(b_path.display().to_string()),
        };
        assert_eq!(waiting.new, expected);
        assert_eq!(bound(tmux), before);

        retry_deferred_claude_adoptions();
        let (pending_seq, source) = (waiting.seq, source(&b, &b_path));
        let resolved = BindingTarget::Resolved {
            pending_seq,
            source,
        };
        assert_eq!(last(channel), resolved);
        assert!(pinned(channel, tmux));
        assert_eq!(bound(tmux), before, "binding and cursor kept");
    }

    #[test]
    fn an_unreadable_pinned_current_refuses_the_hook_and_keeps_its_binding() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b) = (7_795, "n2a-pinned-unreadable", uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, Some(&first_row(&a)));
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        let registered = TuiRuntimeBinding {
            last_offset: 12,
            ..claude(&b_path, &b)
        };
        register_tmux_runtime_binding(tmux, registered.clone());
        assert!(adopt(&a, &b, &stop(&b_path)).0.is_some());
        assert!(pinned(channel, tmux));
        let logged = log(channel).len();
        let payload = serde_json::json!({ "session_id": b, "transcript_path": b_path });
        let observe = || {
            let headers = axum::http::HeaderMap::new();
            observe_binding_hook("claude", "stop", Some(&a), Some(&b), &payload, &headers)
        };

        readable(&b_path, false);
        let refused = observe();
        readable(&b_path, true);
        assert_eq!(
            refused,
            IngressOutcome::Unavailable(UnavailableReason::SourceUnreadable)
        );
        assert_eq!(bound(tmux), registered, "binding and cursor kept");
        assert_eq!(log(channel).len(), logged, "nothing is logged");
        assert!(pinned(channel, tmux));

        assert_eq!(observe(), IngressOutcome::Durable(DurableKind::Adopted));
        assert_eq!(bound(tmux), registered);
        assert_eq!(log(channel).len(), logged);
    }

    type Hook = std::rc::Rc<dyn Fn() -> AdoptionHttp>;

    /// Runs `hook` at the restore's check-to-record seam; the slot holds its answer, or `None` when
    /// the restore's authority held it off and it runs after the restore, as a waiting hook does.
    fn hook_at_restore_seam(
        hook: Hook,
    ) -> std::rc::Rc<std::cell::RefCell<Option<Option<AdoptionHttp>>>> {
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let seam_slot = slot.clone();
        let seam = move || {
            let mut answer = None;
            let held_off = tc::source_authority_contention_key_for_tests(|| answer = Some(hook()));
            *seam_slot.borrow_mut() = Some(answer.filter(|_| held_off.is_none()));
        };
        AFTER_CHECK.with_borrow_mut(|after| *after = Some(Box::new(seam)));
        slot
    }

    /// The pane's bound source, its pin and its log's current all name `session`'s `path`, and no
    /// record after `since` names `left` again.
    fn settled_on(
        channel: u64,
        tmux: &str,
        (session, path): (&str, &Path),
        left: &str,
        since: u64,
    ) {
        let binding = bound(tmux);
        assert_eq!(binding.session_id.as_deref(), Some(session));
        assert_eq!(binding.output_path, path.display().to_string());
        forget_channel_for_tests(channel);
        assert_eq!(
            pinned_source(channel, tmux).unwrap(),
            Some(source(session, path))
        );
        let records = log(channel);
        let current = records.iter().rev().find_map(|r| match &r.new {
            BindingTarget::Source(s) | BindingTarget::Resolved { source: s, .. } => Some(s),
            _ => None,
        });
        assert_eq!(current.map(|s| s.session_id.as_str()), Some(session));
        let back = records.iter().filter(|r| r.seq > since).any(|r| {
            matches!(&r.new, BindingTarget::Source(s) | BindingTarget::Resolved { source: s, .. } if s.session_id == left)
        });
        assert!(!back, "nothing re-registers {left} after the adoption");
    }

    #[test]
    fn a_hook_adopting_c_while_the_restore_rechecks_b_keeps_c_after_polls_and_a_restart() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b, c) = (7_790, "n2a-restore-hook-race", uuid(), uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, None);
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, None);
        let clear_b = signal("session_start", Some("clear"), Some(&b_path));
        assert_eq!(adopt(&a, &b, &clear_b), (None, None));
        at(home.path(), "-work-a", &b, Some(&first_row(&b)));
        // The first poll resolves B; the next one judges B again from the log's new seq.
        let first = restore(channel, tmux, &a, &a_path);
        assert!(
            matches!(
                first,
                PendingRestore::BoundFromLedger {
                    exact_wait: None,
                    ..
                }
            ),
            "{first:?}"
        );
        let resolved = log(channel).pop().unwrap().seq;
        let older = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&b_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let c_path = at(home.path(), "-work-a", &c, Some(&first_row(&c)));
        let clear_c = signal("session_start", Some("clear"), Some(&c_path));
        let (ha, hc) = (a.clone(), c.clone());
        let hook: Hook = std::rc::Rc::new(move || adopt_from_hook(&ha, &hc, &clear_c));
        let seam = hook_at_restore_seam(hook.clone());

        restore(channel, tmux, &a, &a_path);
        let answer = seam
            .borrow_mut()
            .take()
            .expect("the restore reached its record");
        let answer = answer.unwrap_or_else(|| hook());
        assert_eq!(answer, AdoptionHttp::Durable(DurableKind::Adopted));
        settled_on(channel, tmux, (&c, &c_path), &b, resolved);

        let polled = restore(channel, tmux, &a, &a_path);
        assert!(
            matches!(
                polled,
                PendingRestore::BoundFromLedger {
                    exact_wait: None,
                    ..
                }
            ),
            "{polled:?}"
        );
        settled_on(channel, tmux, (&c, &c_path), &b, resolved);

        restart(channel);
        let restarted = restore(channel, tmux, &a, &a_path);
        assert!(
            matches!(
                restarted,
                PendingRestore::BoundFromLedger {
                    exact_wait: None,
                    ..
                }
            ),
            "{restarted:?}"
        );
        settled_on(channel, tmux, (&c, &c_path), &b, resolved);
    }

    #[test]
    fn a_hook_adopting_c_while_the_restore_publishes_an_unwritten_b_keeps_c() {
        let _lane = Lane::new();
        let home = tempfile::tempdir().unwrap();
        let (channel, tmux, a, b, c) = (7_795, "n2a-restore-await-race", uuid(), uuid(), uuid());
        let a_path = at(home.path(), "-work-a", &a, None);
        pane(channel, tmux, &a, &a_path);
        let b_path = at(home.path(), "-work-a", &b, None);
        let clear_b = signal("session_start", Some("clear"), Some(&b_path));
        assert_eq!(adopt(&a, &b, &clear_b), (None, None));
        let pending = log(channel).pop().unwrap().seq;
        let c_path = at(home.path(), "-work-a", &c, Some(&first_row(&c)));
        let clear_c = signal("session_start", Some("clear"), Some(&c_path));
        let (ha, hc) = (a.clone(), c.clone());
        let hook: Hook = std::rc::Rc::new(move || adopt_from_hook(&ha, &hc, &clear_c));
        let seam = hook_at_restore_seam(hook.clone());

        // B has no transcript yet, so the restore publishes it unverified, waiting on its path.
        restore(channel, tmux, &a, &a_path);
        let answer = seam
            .borrow_mut()
            .take()
            .expect("the restore reached its record");
        let answer = answer.unwrap_or_else(|| hook());
        assert_eq!(answer, AdoptionHttp::Durable(DurableKind::Adopted));
        settled_on(channel, tmux, (&c, &c_path), &b, pending);

        let polled = restore(channel, tmux, &a, &a_path);
        assert!(
            matches!(
                polled,
                PendingRestore::BoundFromLedger {
                    exact_wait: None,
                    ..
                }
            ),
            "{polled:?}"
        );
        settled_on(channel, tmux, (&c, &c_path), &b, pending);
    }
}

#[path = "pending_history_tests.rs"]
mod history;
