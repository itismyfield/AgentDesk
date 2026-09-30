//! The rehydrate pass restores a durable Pending after a restart with no hook to redo it.
use super::*;
use crate::services::claude_tui::hook_server::adoption_retry::{
    deferred_adoption_count, reset_deferred_adoptions_for_tests,
};
use crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions;
use crate::services::tui_prompt_dedupe::pending::{
    CHANNEL_MAPPING_TTL, ExactPathWait, PendingRestore, age_channel_mapping_for_tests,
    expire_channel_mapping_for_tests, last_restore_outcome, reset_restore_outcomes_for_tests,
};
use crate::services::tui_prompt_dedupe::{
    claude_session_rotation_for_tmux, register_launched_tmux_runtime_binding,
    resolve_tmux_session_name,
};

/// A live pane launched on A that took a /clear to B before B's transcript existed.
struct Pane {
    tmux: String,
    channel: u64,
    a: String,
    b: String,
    dir: PathBuf,
    shared: Arc<SharedData>,
    _claude_home: crate::config::TestEnvVarGuard,
}

impl Pane {
    fn new(ingress: &Ingress, root: &Path, channel: u64, a_exists: bool) -> Self {
        use crate::services::tmux_common as tc;
        let (tmux, a, b) = (format!("restore-{}", uuid()), uuid(), uuid());
        let home = root.join("claude-home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let claude_home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CLAUDE_CONFIG_DIR",
            &home,
        );
        let a_path =
            crate::services::claude_tui::transcript_tail::claude_transcript_path(&cwd, &a, None)
                .unwrap();
        let dir = a_path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        if a_exists {
            std::fs::write(&a_path, "{}\n").unwrap();
        }
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel),
            owner_runtime_root: root.display().to_string(),
            host: None,
            expected_native_session_id: Some(a.clone()),
            launch_mode: "fresh".into(),
            provider_root: Some(home.clone()),
        };
        let prepared = PreparedIncarnation::create(context).unwrap();
        let script = tc::session_temp_path(&tmux, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
        std::fs::create_dir_all(Path::new(&script).parent().unwrap()).unwrap();
        let exec = format!(
            "cd '{}'\nexec 'claude' '--session-id' '{a}'\n",
            cwd.display()
        );
        std::fs::write(&script, format!("{}{exec}", prepared.env_lines())).unwrap();
        let nonce = &prepared.context.execution_nonce;
        std::fs::write(tc::session_temp_path(&tmux, "spawn_nonce"), nonce).unwrap();
        VIEW.with_borrow_mut(|v| {
            *v = Some(View {
                tmux: tmux.clone(),
                channel,
                home,
                peers: Vec::new(),
            })
        });
        dedupe::register_tmux_channel(&tmux, channel);
        dedupe::register_provider_session("claude", &a, &tmux);
        register_launched_tmux_runtime_binding(&tmux, claude(&a_path, &a));
        let pane = Self {
            tmux,
            channel,
            a,
            b,
            dir,
            shared: crate::services::discord::make_shared_data_for_tests(),
            _claude_home: claude_home,
        };
        let clear = serde_json::json!({
            "session_id": pane.b, "source": "clear", "transcript_path": pane.path(&pane.b),
        });
        let status = ingress.claude_hook("SessionStart", &pane.a, &clear, Some(&uuid()));
        assert_eq!(status, 202, "/clear to a file-less B is acknowledged");
        assert_eq!(pending_lines(channel, &pane.b), 1);
        pane
    }

    fn path(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{session}.jsonl"))
    }

    fn touch(&self, session: &str) -> PathBuf {
        let path = self.path(session);
        std::fs::write(&path, "{}\n").unwrap();
        path
    }

    /// What a dcserver restart forgets: every binding, queue, restore outcome and cached writer.
    fn restart(&self) {
        forget_channel_for_tests(self.channel);
        dedupe::reset_state_for_tests();
        reset_deferred_adoptions_for_tests();
        reset_restore_outcomes_for_tests();
    }

    fn rehydrate(&self) -> Option<PendingRestore> {
        super::super::rehydrate_claude_tui_pane(&self.shared, &self.tmux);
        last_restore_outcome(&self.tmux)
    }

    fn bound(&self) -> Option<(String, Option<String>)> {
        dedupe::runtime_binding_for_tmux_session(&self.tmux).map(|b| (b.output_path, b.session_id))
    }

    fn expect_bound(&self, session: &str) {
        let expected = (
            self.path(session).display().to_string(),
            Some(session.to_owned()),
        );
        assert_eq!(self.bound(), Some(expected), "bound to {session}");
        let alias = resolve_tmux_session_name("claude", &self.a);
        assert_eq!(
            alias.as_deref(),
            Some(self.tmux.as_str()),
            "launch A maps to the pane"
        );
    }

    /// Sends the file-less C's /clear with `request` and returns the hook's status.
    fn clear_to(&self, ingress: &Ingress, c: &str, request: &str) -> u16 {
        let clear = serde_json::json!({
            "session_id": c, "source": "clear", "transcript_path": self.path(c),
        });
        ingress.claude_hook("SessionStart", &self.a, &clear, Some(request))
    }

    /// Writes C's transcript a minute newer than `bound`'s and lets the deferred poll adopt it.
    fn adopt_newer(&self, c: &str, bound: &str) {
        let bound = std::fs::metadata(self.path(bound))
            .unwrap()
            .modified()
            .unwrap();
        let c_file = std::fs::File::options()
            .write(true)
            .open(self.touch(c))
            .unwrap();
        c_file
            .set_modified(bound + std::time::Duration::from_secs(60))
            .unwrap();
        retry_deferred_claude_adoptions();
        self.expect_bound(c);
    }

    fn last_record(&self) -> BindingTarget {
        forget_channel_for_tests(self.channel);
        let log = records_strict(self.channel).unwrap().unwrap();
        log.last().unwrap().new.clone()
    }
}

fn names(record: &BindingEvent, session: &str) -> bool {
    match &record.new {
        BindingTarget::Source(s) | BindingTarget::Resolved { source: s, .. } => {
            s.session_id == session
        }
        _ => false,
    }
}

#[test]
fn a_pending_b_is_bound_by_the_pass_when_launch_a_never_existed() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_520, false);
    assert_eq!(
        pending_lines(pane.channel, &pane.a),
        1,
        "launch A is Pending"
    );
    pane.restart();
    pane.touch(&pane.b);

    let bound = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: None,
    };
    assert_eq!(
        pane.rehydrate(),
        Some(bound.clone()),
        "B restored from the ledger"
    );
    assert!(matches!(
        pane.last_record(),
        BindingTarget::Resolved { pending_seq: 2, .. }
    ));
    pane.expect_bound(&pane.b);
    assert_eq!(pending_lines(pane.channel, &pane.a), 1, "no new Pending A");

    // The Resolved B is itself restorable: a second restart with no hook binds B again.
    pane.restart();
    let logged = events(pane.channel).len();
    assert_eq!(pane.rehydrate(), Some(bound), "Resolved B restored");
    pane.expect_bound(&pane.b);
    assert_eq!(events(pane.channel).len(), logged, "nothing new logged");
}

#[test]
fn a_file_less_restored_b_waits_for_its_own_file_and_never_a_newer_one() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_521, false);
    pane.restart();
    let logged = events(pane.channel).len();

    let wait = ExactPathWait {
        session_id: pane.b.clone(),
        transcript: pane.path(&pane.b),
    };
    let waiting = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: Some(wait),
    };
    assert_eq!(
        pane.rehydrate(),
        Some(waiting),
        "B bound before its file exists"
    );
    assert_eq!(events(pane.channel).len(), logged, "nothing logged yet");
    pane.expect_bound(&pane.b);

    // Another session's transcript in the same project is newer, yet the idle reader keeps B.
    let x = uuid();
    pane.touch(&x);
    let binding = dedupe::runtime_binding_for_tmux_session(&pane.tmux).unwrap();
    let channel = ChannelId::new(pane.channel);
    let read =
        resolved_claude_idle_relay_transcript_path(&pane.shared, &pane.tmux, channel, &binding);
    assert_eq!(
        read,
        Some(pane.path(&pane.b)),
        "the idle reader never reads X"
    );
    pane.expect_bound(&pane.b);
    assert!(
        !events(pane.channel).iter().any(|e| names(e, &x)),
        "X never logged"
    );

    pane.touch(&pane.b);
    let resolved = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: None,
    };
    assert_eq!(
        pane.rehydrate(),
        Some(resolved),
        "B resolved once it exists"
    );
    let BindingTarget::Resolved {
        pending_seq,
        source,
    } = pane.last_record()
    else {
        panic!("B is resolved in the log");
    };
    assert_eq!((pending_seq, source.session_id), (2, pane.b.clone()));
    pane.expect_bound(&pane.b);
}

#[test]
fn a_pending_b_behind_an_existing_launch_a_is_seeded_after_a_is_registered() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_522, true);
    pane.restart();

    let seeded = PendingRestore::Seeded { pending_seq: 2 };
    assert_eq!(pane.rehydrate(), Some(seeded), "B seeded behind launch A");
    pane.expect_bound(&pane.a);
    assert_eq!(
        deferred_adoption_count(),
        1,
        "B waits in the adoption queue"
    );

    pane.touch(&pane.b);
    retry_deferred_claude_adoptions();
    assert!(matches!(
        pane.last_record(),
        BindingTarget::Resolved { pending_seq: 2, .. }
    ));
    pane.expect_bound(&pane.b);
    let rotation = claude_session_rotation_for_tmux(&pane.tmux).expect("A to B rotation");
    assert_eq!(
        (rotation.old_session_id, rotation.new_session_id),
        (Some(pane.a.clone()), pane.b.clone())
    );
}

#[test]
fn a_corrupt_log_keeps_the_pass_from_registering_launch_a() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let logs = tempfile::tempdir().unwrap();
    set_test_root(Some(logs.path()));
    let pane = Pane::new(&ingress, root.path(), 7_523, true);
    pane.restart();
    let path = logs.path().join(BINDING_EVENTS_DIR).join("7523.log");
    let text = std::fs::read_to_string(&path).unwrap();
    let first = text.lines().next().unwrap().to_owned();
    std::fs::write(&path, format!("{first}\n{{not json\n")).unwrap();

    let outcome = pane.rehydrate();
    assert!(
        matches!(outcome, Some(PendingRestore::BlockedCorrupt(_))),
        "{outcome:?}"
    );
    assert_eq!(
        pane.bound(),
        None,
        "the pass registers nothing over a corrupt log"
    );
}

#[test]
fn a_resolved_b_stays_bound_over_launch_a_on_every_later_pass() {
    use crate::services::tmux_common as tc;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_524, true);
    let script = tc::session_temp_path(&pane.tmux, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
    let launch_a = std::fs::read_to_string(&script).unwrap();
    pane.touch(&pane.b);
    retry_deferred_claude_adoptions();
    assert!(matches!(
        pane.last_record(),
        BindingTarget::Resolved { pending_seq: 2, .. }
    ));
    // A restart after Resolved B but before the launch artifact moved to B, with no hook memory.
    std::fs::write(&script, launch_a).unwrap();
    pane.restart();
    dedupe::forget_hook_adopted_claude_session_id(&pane.tmux);
    dedupe::clear_claude_session_rotation(&pane.tmux);
    let logged = events(pane.channel).len();

    let bound = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: None,
    };
    assert_eq!(pane.rehydrate(), Some(bound), "B restored from the ledger");
    pane.expect_bound(&pane.b);
    pane.rehydrate();
    pane.expect_bound(&pane.b);
    assert_eq!(
        events(pane.channel).len(),
        logged,
        "launch A never re-registered"
    );
}

#[test]
fn a_file_less_restored_b_gives_way_to_the_next_session_with_a_file() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_525, false);
    pane.restart();
    let waiting = pane.rehydrate();
    assert!(
        matches!(
            waiting,
            Some(PendingRestore::BoundFromLedger {
                exact_wait: Some(_),
                ..
            })
        ),
        "{waiting:?}"
    );

    // B never gets a file; the pane moves on to C, whose transcript exists before its hook lands.
    let c = uuid();
    pane.touch(&c);
    let clear = serde_json::json!({
        "session_id": c, "source": "clear", "transcript_path": pane.path(&c),
    });
    let status = ingress.claude_hook("SessionStart", &pane.a, &clear, Some(&uuid()));
    assert_eq!(status, 202, "C is not refused over B's missing file");
    pane.expect_bound(&c);
    pane.rehydrate();
    pane.expect_bound(&c);
}

#[test]
fn a_restored_b_whose_channel_mapping_expired_still_logs_and_adopts_a_file_less_c() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_526, false);
    pane.restart();
    pane.touch(&pane.b);
    let bound = PendingRestore::BoundFromLedger {
        pending_seq: 2,
        exact_wait: None,
    };
    assert_eq!(pane.rehydrate(), Some(bound.clone()), "B restored");

    // The idle reader keeps B's runtime binding fresh; only the channel mapping outlives its TTL.
    // The next pass judges B again (its Resolved moved the log on), the one after hits the memo.
    for pass in ["judged again", "memoized"] {
        expire_channel_mapping_for_tests(&pane.tmux);
        assert_eq!(dedupe::owner_channel_for_tmux_session(&pane.tmux), None);
        assert_eq!(pane.rehydrate(), Some(bound.clone()), "B kept when {pass}");
        let mapped = dedupe::owner_channel_for_tmux_session(&pane.tmux);
        assert_eq!(mapped, Some(pane.channel), "mapping renewed when {pass}");
    }
    pane.expect_bound(&pane.b);

    let c = uuid();
    let status = pane.clear_to(&ingress, &c, &uuid());
    assert_eq!(status, 202, "/clear to a file-less C is acknowledged");
    assert_eq!(
        pending_lines(pane.channel, &c),
        1,
        "C is Pending in the log"
    );
    assert_eq!(
        deferred_adoption_count(),
        1,
        "C waits in the adoption queue"
    );
    pane.adopt_newer(&c, &pane.b);
}

#[test]
fn a_seeded_pane_keeps_its_channel_and_refuses_a_file_less_c_until_the_pass_restores_it() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_528, true);
    pane.restart();
    let seeded = PendingRestore::Seeded { pending_seq: 2 };
    assert_eq!(pane.rehydrate(), Some(seeded.clone()), "B seeded");
    pane.adopt_newer(&pane.b, &pane.a);
    // The delivery path settles the A to B rotation; the next poll retires B's entry.
    dedupe::clear_claude_session_rotation(&pane.tmux);
    retry_deferred_claude_adoptions();
    assert_eq!(deferred_adoption_count(), 0, "B's entry retired");

    // Later passes over the Seeded pane renew a mapping that would lapse before the next one.
    age_channel_mapping_for_tests(
        &pane.tmux,
        CHANNEL_MAPPING_TTL - std::time::Duration::from_secs(30),
    );
    assert_eq!(pane.rehydrate(), Some(seeded), "the outcome stays Seeded");
    pane.expect_bound(&pane.b);
    age_channel_mapping_for_tests(&pane.tmux, std::time::Duration::from_secs(60));
    let mapped = dedupe::owner_channel_for_tmux_session(&pane.tmux);
    assert_eq!(mapped, Some(pane.channel), "the pass renewed the mapping");

    // A mapping that lapsed anyway refuses the file-less C instead of dropping it.
    expire_channel_mapping_for_tests(&pane.tmux);
    let (c, request) = (uuid(), uuid());
    let status = pane.clear_to(&ingress, &c, &request);
    assert_eq!(status, 425, "C refused while its pane has no channel");
    assert_eq!(pending_lines(pane.channel, &c), 0, "nothing logged for C");
    pane.rehydrate();
    let status = pane.clear_to(&ingress, &c, &request);
    assert_eq!(status, 202, "the retry after the pass is acknowledged");
    assert_eq!(
        pending_lines(pane.channel, &c),
        1,
        "C is Pending in the log"
    );
    assert_eq!(
        deferred_adoption_count(),
        1,
        "C waits in the adoption queue"
    );
    pane.adopt_newer(&c, &pane.b);
}

#[test]
fn a_pane_no_pass_mapped_still_acknowledges_a_file_less_hook_without_a_channel() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_530, true);
    assert_eq!(
        last_restore_outcome(&pane.tmux),
        None,
        "no pass reached the pane"
    );

    // No pass would restore this mapping, so a refusal would only stall the pane's hooks.
    expire_channel_mapping_for_tests(&pane.tmux);
    let c = uuid();
    assert_eq!(
        pane.clear_to(&ingress, &c, &uuid()),
        202,
        "C proceeds unlogged"
    );
    assert_eq!(pending_lines(pane.channel, &c), 0, "nothing logged for C");
}

#[test]
fn a_restored_b_whose_transcript_is_unreadable_still_refuses_the_next_session() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_527, false);
    pane.restart();
    // B's path exists but cannot be read; only a missing file counts as older than every candidate.
    std::os::unix::fs::symlink(pane.path(&pane.b), pane.path(&pane.b)).unwrap();
    let waiting = pane.rehydrate();
    assert!(
        matches!(
            waiting,
            Some(PendingRestore::BoundFromLedger {
                exact_wait: Some(_),
                ..
            })
        ),
        "{waiting:?}"
    );

    let c = uuid();
    pane.touch(&c);
    let clear = serde_json::json!({
        "session_id": c, "source": "clear", "transcript_path": pane.path(&c),
    });
    let status = ingress.claude_hook("SessionStart", &pane.a, &clear, Some(&uuid()));
    assert_eq!(status, 425, "an unreadable B is not skipped as missing");
    pane.expect_bound(&pane.b);
}

#[test]
fn a_deferred_b_outlives_a_poll_that_finds_its_channel_mapping_lapsed() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let ingress = Ingress::new();
    let _reset = Reset;
    let pane = Pane::new(&ingress, root.path(), 7_529, true);
    let seeded = PendingRestore::Seeded { pending_seq: 2 };
    assert_eq!(pane.rehydrate(), Some(seeded), "the pass maps the pane");

    // The poll runs before the pass renews the mapping, so it can find the mapping lapsed.
    expire_channel_mapping_for_tests(&pane.tmux);
    retry_deferred_claude_adoptions();
    assert_eq!(deferred_adoption_count(), 1, "B keeps its place");
    pane.rehydrate();
    let mapped = dedupe::owner_channel_for_tmux_session(&pane.tmux);
    assert_eq!(mapped, Some(pane.channel), "the pass restored the mapping");
    pane.adopt_newer(&pane.b, &pane.a);
}
