use super::*;
use crate::services::tui_input::actor::capability::{AttachEvidence, FoldProfile};
use crate::services::tui_input::actor::resume::{
    Absence, ActiveEvidence, EOF_INTERVAL, ResumeEvidence, SETTLE_WINDOW, StablePrefix,
};
use crate::services::tui_input::attempt::{Witness, WitnessKind};
use sha2::{Digest, Sha256};

const NEW_NONCE: &str = "resumed-nonce";

fn attach_evidence(binding: &SourceBinding, nonce: &str) -> AttachEvidence {
    AttachEvidence {
        binding: binding.clone(),
        execution_nonce: nonce.into(),
        launch_nonce: Some(nonce.into()),
        version: Some("2.1.295".into()),
        rows: 24,
        fold_profile: Some(FoldProfile::measured(24)),
        actual_hooks: true,
        source_verified: true,
        pane_process_live: true,
        gate_verified: true,
        permission_mode: Some("auto".into()),
    }
}

fn enter_count(tmux: &FakeTmux) -> usize {
    tmux.calls()
        .iter()
        .filter(|call| *call == "send-keys")
        .count()
}

fn pane_with_nonce(tmux: &FakeTmux, nonce: &str) -> TmuxPane {
    let mut pane = tmux.pane();
    pane.attest_test_nonce(nonce);
    pane
}

fn folded_tmux() -> FakeTmux {
    let tmux = FakeTmux::new(
        CLAUDE_READY,
        r#"paste-buffer)
  dir=$(dirname "$0")
  lines=$(LC_ALL=C tr -cd '\n' < "$dir/buffer" | wc -c | tr -d ' ')
  printf '────────────────────\n❯ [Pasted text #1 +%s lines]\n────────────────────\n' "$lines" > "$dir/screen"
  ;;
send-keys) cp "$(dirname "$0")/ready" "$(dirname "$0")/screen" ;;"#,
    );
    fs::write(tmux.dir.path().join("ready"), CLAUDE_READY).unwrap();
    tmux
}

struct ResumeFixture {
    world: World,
    old_tmux: FakeTmux,
    new_tmux: FakeTmux,
    binding: SourceBinding,
    ledger: Ledger,
    actor: InputActor<TmuxPane>,
    frame: String,
    evidence: ResumeEvidence,
    now: Instant,
}

async fn fixture(new_source: bool, hook_only: bool) -> ResumeFixture {
    let world = World::new(ShadowProvider::Claude);
    let old_tmux = folded_tmux();
    let mut ledger = world.ledger(&[(1, "resume this queued input")]);
    let mut old_actor = InputActor::attach(
        world.binding.clone(),
        old_tmux.pane(),
        attach_evidence(&world.binding, "test-nonce"),
    );
    let fact = world.fact(open_turn());
    assert_eq!(
        old_actor
            .step(&mut ledger, Some(&fact), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    let rows = ledger.rows().unwrap();
    let row = rows.row(1).unwrap();
    let frame = row.attempt.as_ref().unwrap().rendered_prompt.clone();
    if hook_only {
        ledger
            .append_witness(
                1,
                Witness {
                    generation: 1,
                    token: row.attempts[0].token.clone(),
                    kind: WitnessKind::Hook,
                    range: None,
                    record_key: None,
                    turn_ref: None,
                },
            )
            .unwrap();
    } else {
        world.append(json!({"type":"queue-operation","operation":"enqueue","content":frame}));
        assert_eq!(
            old_actor
                .step(&mut ledger, Some(&fact), Instant::now())
                .await
                .unwrap(),
            Step::Blocked(1, RowState::Queued)
        );
    }
    assert_eq!(state_of(&ledger, 1), RowState::Queued);
    assert_eq!(enter_count(&old_tmux), 1);
    let binding = if new_source {
        let path = world._dir.path().join("resumed.jsonl");
        fs::write(&path, "{\"type\":\"summary\"}\n").unwrap();
        SourceBinding {
            source: source_id_for("resumed", &path).unwrap(),
            ..world.binding.clone()
        }
    } else {
        world.binding.clone()
    };
    let new_tmux = folded_tmux();
    let actor = InputActor::attach(
        binding.clone(),
        pane_with_nonce(&new_tmux, NEW_NONCE),
        attach_evidence(&binding, NEW_NONCE),
    );
    let now = Instant::now();
    let bytes = fs::read(&world.transcript).unwrap();
    let first = StablePrefix {
        source: world.binding.source.clone(),
        eof: bytes.len() as u64,
        digest: hex::encode(Sha256::digest(&bytes)),
        complete: true,
        observed_at: now - EOF_INTERVAL,
    };
    let second = StablePrefix {
        observed_at: now,
        ..first.clone()
    };
    let evidence = ResumeEvidence {
        old_nonce: "test-nonce".into(),
        old_pane: Absence::Absent,
        old_pid: Absence::Absent,
        exited_at: Some(first.observed_at),
        stable_prefixes: Some([first, second]),
        active: Some(ActiveEvidence {
            binding: binding.clone(),
            execution_nonce: NEW_NONCE.into(),
            observed_at: now - SETTLE_WINDOW,
        }),
        lineage_complete: true,
        all_generations_clear: true,
        current_valid: true,
        exact_empty: true,
        control_clear: true,
        settle_profile: "claude-2.1.295-eof1-settle5".into(),
    };
    ResumeFixture {
        world,
        old_tmux,
        new_tmux,
        binding,
        ledger,
        actor,
        frame,
        evidence,
        now,
    }
}

async fn queued_pair_fixture() -> ResumeFixture {
    let mut fixture = fixture(true, false).await;
    fixture
        .ledger
        .append_entry(
            &Entry::Received {
                key: 2,
                input: json!({"text": "resume the second queued input"}),
            },
            &[],
        )
        .unwrap();
    let mut old_actor = InputActor::attach(
        fixture.world.binding.clone(),
        fixture.old_tmux.pane(),
        attach_evidence(&fixture.world.binding, "test-nonce"),
    );
    let fact = fixture.world.fact(open_turn());
    assert_eq!(
        old_actor
            .step(&mut fixture.ledger, Some(&fact), fixture.now)
            .await
            .unwrap(),
        Step::Moved(2, RowState::AwaitTurn)
    );
    let rows = fixture.ledger.rows().unwrap();
    let frame = &rows
        .row(2)
        .unwrap()
        .attempt
        .as_ref()
        .unwrap()
        .rendered_prompt;
    fixture
        .world
        .append(json!({"type":"queue-operation","operation":"enqueue","content":frame}));
    assert_eq!(
        old_actor
            .step(&mut fixture.ledger, Some(&fact), fixture.now)
            .await
            .unwrap(),
        Step::Blocked(1, RowState::Queued)
    );
    let bytes = fs::read(&fixture.world.transcript).unwrap();
    for read in fixture.evidence.stable_prefixes.as_mut().unwrap() {
        read.eof = bytes.len() as u64;
        read.digest = hex::encode(Sha256::digest(&bytes));
    }
    fixture.actor = InputActor::attach(
        fixture.binding.clone(),
        pane_with_nonce(&fixture.new_tmux, NEW_NONCE),
        attach_evidence(&fixture.binding, NEW_NONCE),
    );
    for key in [1, 2] {
        assert_eq!(state_of(&fixture.ledger, key), RowState::Queued);
        let rows = fixture.ledger.rows().unwrap();
        let attempt = &rows.row(key).unwrap().attempts[0];
        assert_eq!(attempt.source, fixture.world.binding.source);
        assert_eq!(attempt.execution_nonce, "test-nonce");
    }
    assert_eq!(enter_count(&fixture.old_tmux), 2);
    assert_eq!(enter_count(&fixture.new_tmux), 0);
    fixture
}

async fn assert_queued_pair_recovery(out_of_order: bool) {
    let mut fixture = queued_pair_fixture().await;
    let wal = fixture.ledger.dir.join("wal.0.jsonl");
    if out_of_order {
        let before = fs::read(&wal).unwrap();
        let result = fixture
            .actor
            .resume_queued(&mut fixture.ledger, 2, &fixture.evidence, fixture.now)
            .unwrap();
        assert!(matches!(
            result,
            Step::Blocked(2, RowState::Queued) | Step::Wait(_)
        ));
        assert_eq!(fs::read(&wal).unwrap(), before);
        assert_eq!(state_of(&fixture.ledger, 2), RowState::Queued);
        assert_eq!(enter_count(&fixture.new_tmux), 0);
    }
    assert_eq!(
        fixture
            .actor
            .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    assert_eq!(enter_count(&fixture.new_tmux), 1);
    if out_of_order {
        let before = fs::read(&wal).unwrap();
        let result = fixture
            .actor
            .resume_queued(&mut fixture.ledger, 2, &fixture.evidence, fixture.now)
            .unwrap();
        assert!(matches!(
            result,
            Step::Blocked(2, RowState::Queued) | Step::Wait(_)
        ));
        assert_eq!(fs::read(&wal).unwrap(), before);
        assert_eq!(enter_count(&fixture.new_tmux), 1);
    }
    let rows = fixture.ledger.rows().unwrap();
    let frame = &rows
        .row(1)
        .unwrap()
        .attempt
        .as_ref()
        .unwrap()
        .rendered_prompt;
    append_to(
        &fixture.binding.source.path,
        &json!({"type":"queue-operation","operation":"enqueue","content":frame}),
    );
    let fact = ChannelFact {
        binding: fixture.binding.clone(),
        through: fs::metadata(&fixture.binding.source.path).unwrap().len(),
        state: open_turn(),
    };
    let result = fixture
        .actor
        .step(&mut fixture.ledger, Some(&fact), fixture.now)
        .await
        .unwrap();
    assert!(matches!(
        result,
        Step::Blocked(2, RowState::Queued) | Step::Wait(_)
    ));
    assert_eq!(state_of(&fixture.ledger, 1), RowState::Queued);
    assert_eq!(state_of(&fixture.ledger, 2), RowState::Queued);
    assert_eq!(enter_count(&fixture.new_tmux), 1);
    assert_eq!(
        fixture
            .actor
            .resume_queued(&mut fixture.ledger, 2, &fixture.evidence, fixture.now)
            .unwrap(),
        Step::Moved(2, RowState::AwaitTurn)
    );
    assert_eq!(enter_count(&fixture.new_tmux), 2);
    let rows = fixture.ledger.rows().unwrap();
    for key in [1, 2] {
        let row = rows.row(key).unwrap();
        assert!(!matches!(row.state, RowState::Held(_)));
        assert_eq!(row.attempts.len(), 2);
        assert_eq!(row.attempts[1].generation, 2);
        assert_eq!(row.attempts[1].execution_nonce, NEW_NONCE);
        assert_eq!(row.attempts[1].source, fixture.binding.source);
        assert_eq!(
            row.attempts
                .iter()
                .filter(|attempt| attempt.queue_end.is_some())
                .count(),
            1
        );
    }
    drop(fixture.ledger);
    let mut replay = Ledger::open(&fixture.world.runtime, CHANNEL).unwrap();
    let mut restarted = InputActor::attach(
        fixture.binding.clone(),
        pane_with_nonce(&fixture.new_tmux, NEW_NONCE),
        attach_evidence(&fixture.binding, NEW_NONCE),
    );
    let before = fs::read(&wal).unwrap();
    for key in [1, 2] {
        let state = state_of(&replay, key);
        assert!(!matches!(state, RowState::Held(_)));
        assert_eq!(
            restarted
                .resume_queued(&mut replay, key, &fixture.evidence, fixture.now)
                .unwrap(),
            Step::Blocked(key, state)
        );
        assert_eq!(replay.rows().unwrap().row(key).unwrap().attempts.len(), 2);
    }
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert_eq!(enter_count(&fixture.new_tmux), 2);
}

#[tokio::test]
async fn resume_actor_two_old_queued_rows_survive_current_q_observation_and_replay() {
    assert_queued_pair_recovery(false).await;
}

#[tokio::test]
async fn resume_actor_two_old_queued_rows_wait_out_of_order_without_wal_changes() {
    assert_queued_pair_recovery(true).await;
}

#[tokio::test]
async fn resume_actor_verified_queue_ends_once_at_exact_boundaries_and_wal_replay() {
    for new_source in [false, true] {
        let mut fixture = fixture(new_source, false).await;
        assert_eq!(
            fixture
                .actor
                .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
                .unwrap(),
            Step::Moved(1, RowState::AwaitTurn)
        );
        let rows = fixture.ledger.rows().unwrap();
        let row = rows.row(1).unwrap();
        assert_eq!(row.attempts.len(), 2);
        assert_eq!(row.attempts[1].generation, 2);
        assert_ne!(row.attempts[0].token, row.attempts[1].token);
        assert_eq!(row.attempts[1].execution_nonce, NEW_NONCE);
        assert_eq!(
            row.attempts[1].queue_end.as_ref().unwrap().old_nonce,
            "test-nonce"
        );
        assert_eq!(row.attempts[1].source, fixture.binding.source);
        assert_eq!(enter_count(&fixture.new_tmux), 1);
        assert_eq!(enter_count(&fixture.old_tmux), 1);
        assert_eq!(
            fixture
                .actor
                .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
                .unwrap(),
            Step::Blocked(1, RowState::AwaitTurn)
        );
        assert_eq!(enter_count(&fixture.new_tmux), 1);

        drop(fixture.ledger);
        let mut replay = Ledger::open(&fixture.world.runtime, CHANNEL).unwrap();
        let mut restarted = InputActor::attach(
            fixture.binding.clone(),
            pane_with_nonce(&fixture.new_tmux, NEW_NONCE),
            attach_evidence(&fixture.binding, NEW_NONCE),
        );
        assert_eq!(
            restarted
                .resume_queued(&mut replay, 1, &fixture.evidence, fixture.now)
                .unwrap(),
            Step::Blocked(1, RowState::AwaitTurn)
        );
        assert_eq!(replay.rows().unwrap().row(1).unwrap().attempts.len(), 2);
        assert_eq!(enter_count(&fixture.new_tmux), 1);
    }
}

#[tokio::test]
async fn resume_actor_each_missing_or_unknown_guard_holds_without_enter() {
    type Change = fn(&mut ResumeEvidence);
    let cases: [(&str, Change); 16] = [
        ("old pane present", |e| e.old_pane = Absence::Present),
        ("old pane unknown", |e| e.old_pane = Absence::Unknown),
        ("old PID present", |e| e.old_pid = Absence::Present),
        ("ps decode failure", |e| e.old_pid = Absence::Unknown),
        ("different old episode", |e| e.old_nonce = "other".into()),
        ("exit unknown", |e| e.exited_at = None),
        ("prefix absent", |e| e.stable_prefixes = None),
        ("prefix incomplete", |e| {
            e.stable_prefixes.as_mut().unwrap()[0].complete = false
        }),
        ("EOF reads less than 1 second apart", |e| {
            e.stable_prefixes.as_mut().unwrap()[0].observed_at += Duration::from_nanos(1)
        }),
        ("missing ACTIVE", |e| e.active = None),
        ("settle below 5 seconds", |e| {
            e.active.as_mut().unwrap().observed_at += Duration::from_millis(1)
        }),
        ("lineage unknown", |e| e.lineage_complete = false),
        ("all generation scan unknown", |e| {
            e.all_generations_clear = false
        }),
        ("current invalid", |e| e.current_valid = false),
        ("composer not exact-empty", |e| e.exact_empty = false),
        ("control fence", |e| e.control_clear = false),
    ];
    for (name, change) in cases {
        let mut fixture = fixture(false, false).await;
        change(&mut fixture.evidence);
        assert_eq!(
            fixture
                .actor
                .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
                .unwrap(),
            Step::Moved(1, RowState::Held(HeldReason::Ambiguous)),
            "{name}"
        );
        assert_eq!(enter_count(&fixture.new_tmux), 0, "{name}");
        assert_eq!(
            state_of(&fixture.ledger, 1),
            RowState::Held(HeldReason::Ambiguous),
            "{name}"
        );
        assert_eq!(
            fixture
                .ledger
                .rows()
                .unwrap()
                .row(1)
                .unwrap()
                .attempts
                .len(),
            1,
            "{name}"
        );
        assert_eq!(owner_of(&fixture.ledger, 1), Owner::Ledger, "{name}");
    }
}

#[tokio::test]
async fn resume_actor_hook_only_is_held_even_after_settle() {
    let mut fixture = fixture(false, true).await;
    assert_eq!(
        fixture
            .actor
            .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
            .unwrap(),
        Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
    );
    assert_eq!(enter_count(&fixture.new_tmux), 0);
    assert_eq!(
        fixture
            .ledger
            .rows()
            .unwrap()
            .row(1)
            .unwrap()
            .attempts
            .len(),
        1
    );
}

#[tokio::test]
async fn resume_actor_changed_composer_holds_without_enter() {
    let mut fixture = fixture(false, false).await;
    fs::write(
        fixture.new_tmux.dir.path().join("screen"),
        "────────────────────\n❯ occupied\n────────────────────\n",
    )
    .unwrap();
    assert_eq!(
        fixture
            .actor
            .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
            .unwrap(),
        Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
    );
    assert_eq!(enter_count(&fixture.new_tmux), 0);
    assert_eq!(
        state_of(&fixture.ledger, 1),
        RowState::Held(HeldReason::Ambiguous)
    );
    assert_eq!(
        fixture
            .ledger
            .rows()
            .unwrap()
            .row(1)
            .unwrap()
            .attempts
            .len(),
        1
    );
}

#[tokio::test]
async fn resume_actor_old_q_does_not_mask_new_hook_only_watchdog() {
    let mut fixture = fixture(false, false).await;
    assert_eq!(
        fixture
            .actor
            .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    let rows = fixture.ledger.rows().unwrap();
    let latest = rows.row(1).unwrap().attempts.last().unwrap();
    fixture
        .ledger
        .append_witness(
            1,
            Witness {
                generation: latest.generation,
                token: latest.token.clone(),
                kind: WitnessKind::Hook,
                range: None,
                record_key: None,
                turn_ref: None,
            },
        )
        .unwrap();
    let entered = fixture.actor.entered_at();
    assert_eq!(
        fixture
            .actor
            .step(
                &mut fixture.ledger,
                Some(&fixture.world.fact(open_turn())),
                entered + ACCEPT_WINDOW,
            )
            .await
            .unwrap(),
        Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
    );
    assert_eq!(enter_count(&fixture.new_tmux), 1);
}

fn append_to(path: &Path, record: &Value) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{record}").unwrap();
}

#[tokio::test]
async fn resume_actor_refreshes_old_and_new_source_model_witnesses_before_enter() {
    for old_source in [true, false] {
        for attachment in [false, true] {
            let mut fixture = fixture(true, false).await;
            let record = if attachment {
                json!({"type":"attachment","uuid":"late-attachment","attachment": {
                    "type":"queued_command","commandMode":"prompt","prompt":fixture.frame}})
            } else {
                json!({"type":"user","uuid":"late-user","message":{"role":"user","content":fixture.frame}})
            };
            let path = if old_source {
                &fixture.world.transcript
            } else {
                &fixture.binding.source.path
            };
            append_to(path, &record);
            assert_eq!(
                fixture
                    .actor
                    .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
                    .unwrap(),
                Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
            );
            assert_eq!(enter_count(&fixture.new_tmux), 0);
            let rows = fixture.ledger.rows().unwrap();
            let row = rows.row(1).unwrap();
            assert_eq!(row.attempts.len(), 1);
            assert!(row.witnesses.iter().any(|seen| {
                seen.witness.generation == 1 && seen.witness.kind.confirms_input()
            }));
        }
    }
}

#[tokio::test]
async fn resume_actor_checks_same_owner_prefix_again_after_composer_capture() {
    let mut fixture = fixture(false, false).await;
    let program = fixture.new_tmux.dir.path().join("tmux");
    let script = fs::read_to_string(&program).unwrap();
    let late = fixture.new_tmux.dir.path().join("late-record");
    let marker = fixture.new_tmux.dir.path().join("late-record-appended");
    fs::write(&late, format!("{}\n", json!({"type":"user","uuid":"capture-race", "message":{"role":"user","content":fixture.frame}}))).unwrap();
    let action = format!(
        "capture-pane) if [ ! -f '{}' ]; then cat '{}' >> '{}'; : > '{}'; fi;",
        marker.display(),
        late.display(),
        fixture.world.transcript.display(),
        marker.display(),
    );
    fs::write(&program, script.replace("capture-pane)", &action)).unwrap();
    assert_eq!(
        fixture
            .actor
            .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
            .unwrap(),
        Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
    );
    assert_eq!(enter_count(&fixture.new_tmux), 0);
    let rows = fixture.ledger.rows().unwrap();
    assert_eq!(
        rows.row(1)
            .unwrap()
            .witnesses
            .iter()
            .filter(|seen| { seen.witness.kind == WitnessKind::User })
            .count(),
        1
    );
    assert_eq!(
        fixture
            .ledger
            .rows()
            .unwrap()
            .row(1)
            .unwrap()
            .attempts
            .len(),
        1
    );
}

#[tokio::test]
async fn resume_actor_partial_tail_and_stale_digest_fail_closed() {
    for partial in [false, true] {
        let mut fixture = fixture(false, false).await;
        if partial {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&fixture.world.transcript)
                .unwrap();
            write!(file, "{{\"type\":\"user\"").unwrap();
        } else {
            fixture.evidence.stable_prefixes.as_mut().unwrap()[1].digest = "0".repeat(64);
        }
        assert_eq!(
            fixture
                .actor
                .resume_queued(&mut fixture.ledger, 1, &fixture.evidence, fixture.now)
                .unwrap(),
            Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
        );
        assert_eq!(enter_count(&fixture.new_tmux), 0);
        assert_eq!(
            fixture
                .ledger
                .rows()
                .unwrap()
                .row(1)
                .unwrap()
                .attempts
                .len(),
            1
        );
    }
}
