use super::*;
use crate::services::tui_input::actor::capability::{
    AttachEvidence, CLAUDE_BUSY_VERSION, FoldProfile,
};
use crate::services::tui_input::actor::token;
use crate::services::tui_input::attempt::{Witness, WitnessKind};

const BUSY_EMPTY: &str = "\
✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)
────────────────────
❯ 
────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";

fn attach_proof(world: &World) -> AttachEvidence {
    AttachEvidence {
        binding: world.binding.clone(),
        execution_nonce: "test-nonce".into(),
        launch_nonce: Some("test-nonce".into()),
        version: Some(CLAUDE_BUSY_VERSION.into()),
        rows: 24,
        fold_profile: Some(FoldProfile::measured(24)),
        actual_hooks: true,
        source_verified: true,
        pane_process_live: true,
        gate_verified: true,
        permission_mode: Some("auto".into()),
    }
}

// The folded composer count comes from bytes accepted by load-buffer, as in Claude's display.
fn folded_tmux() -> FakeTmux {
    let tmux = FakeTmux::new(
        BUSY_EMPTY,
        r#"paste-buffer)
  dir=$(dirname "$0")
  lines=$(LC_ALL=C tr -cd '\n' < "$dir/buffer" | wc -c | tr -d ' ')
  printf '────────────────────\n❯ [Pasted text #1 +%s lines]\n────────────────────\n' "$lines" > "$dir/screen"
  cp "$dir/screen" "$dir/post-paste"
  ;;
send-keys) cp "$(dirname "$0")/ready" "$(dirname "$0")/screen" ;;"#,
    );
    fs::write(tmux.dir.path().join("ready"), BUSY_EMPTY).unwrap();
    tmux
}

fn sent_frame(tmux: &FakeTmux) -> String {
    fs::read_to_string(tmux.dir.path().join("buffer")).unwrap()
}

fn enter_count(tmux: &FakeTmux) -> usize {
    tmux.calls()
        .iter()
        .filter(|call| *call == "send-keys")
        .count()
}

fn enqueue(world: &World, frame: &str) {
    world.append(json!({"type":"queue-operation", "operation":"enqueue", "content":frame}));
}

#[tokio::test]
async fn busy_three_rows_q_only_every_enter_once() {
    let world = World::new(ShadowProvider::Claude);
    let tmux = folded_tmux();
    let mut ledger = world.ledger(&[(1, "first"), (2, "second"), (3, "third")]);
    for key in 1..=3 {
        ledger
            .append_entry(
                &Entry::Transition {
                    key,
                    state: RowState::Ready,
                    attempt: None,
                },
                &[],
            )
            .unwrap();
    }
    let mut actor = InputActor::attach(world.binding.clone(), tmux.pane(), attach_proof(&world));
    let mut frames = Vec::new();
    for key in 1..=3 {
        let step = actor
            .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
            .await
            .unwrap();
        assert_eq!(step, Step::Moved(key, RowState::AwaitTurn));
        assert_eq!(enter_count(&tmux), key as usize);
        for earlier in 1..key {
            assert_eq!(state_of(&ledger, earlier), RowState::Queued);
        }
        let sent = sent_frame(&tmux);
        assert_eq!(
            token::frames(token::profile(ShadowProvider::Claude), &sent).len(),
            1
        );
        enqueue(&world, &sent);
        frames.push(sent);
    }
    let step = actor
        .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
        .await
        .unwrap();
    assert_eq!(step, Step::Blocked(1, RowState::Queued));
    assert_eq!(enter_count(&tmux), 3);
    for key in 1..=3 {
        let rows = ledger.rows().unwrap();
        let row = rows.row(key).unwrap();
        assert_eq!(row.state, RowState::Queued);
        assert_eq!(row.attempts.len(), 1);
        assert_eq!(
            row.attempt.as_ref().unwrap().rendered_prompt,
            frames[(key - 1) as usize]
        );
        assert!(
            !row.witnesses
                .iter()
                .any(|seen| seen.witness.kind.confirms_input())
        );
    }
}

#[tokio::test]
async fn busy_inverted_q_u_rows_keep_queued_and_running_distinct() {
    let world = World::new(ShadowProvider::Claude);
    let tmux = folded_tmux();
    let mut ledger = world.ledger(&[(1, "first"), (2, "second"), (3, "third")]);
    let mut actor = InputActor::attach(world.binding.clone(), tmux.pane(), attach_proof(&world));
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    let first = sent_frame(&tmux);
    enqueue(&world, &first);
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
            .await
            .unwrap(),
        Step::Moved(2, RowState::AwaitTurn)
    );
    let second = sent_frame(&tmux);
    world.user(&second);
    enqueue(&world, &second);
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
            .await
            .unwrap(),
        Step::Moved(3, RowState::AwaitTurn)
    );
    assert_eq!(state_of(&ledger, 1), RowState::Queued);
    assert_eq!(state_of(&ledger, 2), RowState::Running);
    let third = sent_frame(&tmux);
    enqueue(&world, &third);
    world.user(&first);
    actor
        .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
        .await
        .unwrap();
    assert_eq!(state_of(&ledger, 1), RowState::Running);
    assert_eq!(state_of(&ledger, 2), RowState::Running);
    assert_eq!(state_of(&ledger, 3), RowState::Queued);
    assert_eq!(enter_count(&tmux), 3);
}

#[tokio::test]
async fn busy_hook_hint_does_not_offer_younger_row() {
    let world = World::new(ShadowProvider::Claude);
    let tmux = folded_tmux();
    let mut ledger = world.ledger(&[(1, "first"), (2, "second")]);
    let mut actor = InputActor::attach(world.binding.clone(), tmux.pane(), attach_proof(&world));
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    let meta = ledger.rows().unwrap().row(1).unwrap().attempts[0].clone();
    ledger
        .append_witness(
            1,
            Witness {
                generation: meta.generation,
                token: meta.token,
                kind: WitnessKind::Hook,
                range: None,
                record_key: None,
                turn_ref: None,
            },
        )
        .unwrap();
    actor
        .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
        .await
        .unwrap();
    assert_eq!(enter_count(&tmux), 1);
    assert_eq!(state_of(&ledger, 2), RowState::Received);
    assert_ne!(state_of(&ledger, 1), RowState::Running);
}

#[tokio::test]
async fn busy_off_preserves_turn_not_idle_without_tmux_calls() {
    let world = World::new(ShadowProvider::Claude);
    let tmux = folded_tmux();
    let mut ledger = world.ledger(&[(1, "wait for idle")]);
    let mut actor = InputActor::new(world.binding.clone(), tmux.pane());
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
            .await
            .unwrap(),
        Step::Wait("turn_not_idle")
    );
    assert_eq!(state_of(&ledger, 1), RowState::Received);
    assert!(tmux.calls().is_empty());
}

#[tokio::test]
async fn busy_actor_canonicalizes_exact_transmitted_frame_and_fold_count() {
    use sha2::{Digest, Sha256};

    let world = World::new(ShadowProvider::Claude);
    let tmux = folded_tmux();
    let mut ledger = world.ledger(&[(1, "한글\t내용\r\n끝\r\n")]);
    let mut actor = InputActor::attach(world.binding.clone(), tmux.pane(), attach_proof(&world));
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    let rows = ledger.rows().unwrap();
    let row = rows.row(1).unwrap();
    let meta = &row.attempts[0];
    let sent = sent_frame(&tmux);
    let expected = token::render(&meta.token, "[adk:source:1]\n한글    내용\n끝\n\n[adk:end]");
    assert_eq!(sent, expected);
    assert!(!sent.contains(['\r', '\t']));
    assert_eq!(row.attempt.as_ref().unwrap().rendered_prompt, sent);
    assert_eq!(
        meta.frame_digest,
        token::digest("claude-lf-tab4", &sent).unwrap()
    );
    assert_eq!(
        meta.frame_digest,
        hex::encode(Sha256::digest(sent.as_bytes()))
    );
    let capture = fs::read_to_string(tmux.dir.path().join("post-paste")).unwrap();
    assert!(capture.contains("[Pasted text #1 +6 lines]"));
    assert_eq!(enter_count(&tmux), 1);
    enqueue(&world, &sent);
    actor
        .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
        .await
        .unwrap();
    assert_eq!(state_of(&ledger, 1), RowState::Queued);
    assert_eq!(enter_count(&tmux), 1);
}

#[tokio::test]
async fn busy_no_token_modal_results_held_at_exact_watchdog_boundary() {
    // Actual modal tool-result values: they carry no registered input frame.
    let results = [
        r#"{"type":"user","uuid":"6ceb55a1-85b7-43a3-8edb-ed154c7dd966","message":{"role":"user","content":[{"type":"tool_result","content":"Your questions have been answered: \"Choose harmless color\"=\"Blue\". You can now continue with these answers in mind.","tool_use_id":"toolu_01Tgzy2M8yorZhK7RtTWrb66"}]}}"#,
        r#"{"type":"user","uuid":"075420f2-9349-4cce-90e2-200db37e7c7f","message":{"role":"user","content":[{"tool_use_id":"toolu_015MNCJziRjsdsbaeMjyYjrq","type":"tool_result","content":"N5_PERMISSION_PROBE","is_error":false}]}}"#,
    ];
    for result in results {
        let world = World::new(ShadowProvider::Claude);
        let tmux = folded_tmux();
        let mut ledger = world.ledger(&[(1, "preserve this input")]);
        let mut actor =
            InputActor::attach(world.binding.clone(), tmux.pane(), attach_proof(&world));
        assert_eq!(
            actor
                .step(&mut ledger, Some(&world.fact(open_turn())), Instant::now())
                .await
                .unwrap(),
            Step::Moved(1, RowState::AwaitTurn)
        );
        let entered = actor.entered_at();
        world.append(serde_json::from_str(result).unwrap());
        for now in [
            entered - Duration::from_secs(1),
            entered + ACCEPT_WINDOW - Duration::from_nanos(1),
        ] {
            assert_eq!(
                actor
                    .step(&mut ledger, Some(&world.fact(open_turn())), now)
                    .await
                    .unwrap(),
                Step::Wait("awaiting_input_witness")
            );
            assert_eq!(state_of(&ledger, 1), RowState::AwaitTurn);
            assert_eq!(enter_count(&tmux), 1);
        }
        let watchdog = actor
            .step(
                &mut ledger,
                Some(&world.fact(open_turn())),
                entered + ACCEPT_WINDOW,
            )
            .await
            .unwrap();
        assert_eq!(enter_count(&tmux), 1);
        assert_eq!(
            watchdog,
            Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
        );
        for now in [
            entered + ACCEPT_WINDOW,
            entered + ACCEPT_WINDOW + Duration::from_secs(1),
        ] {
            assert_eq!(
                actor
                    .step(&mut ledger, Some(&world.fact(open_turn())), now)
                    .await
                    .unwrap(),
                Step::Blocked(1, RowState::Held(HeldReason::Ambiguous))
            );
        }
        assert_eq!(enter_count(&tmux), 1);
        drop(ledger);
        let restored = Ledger::open(&world.runtime, CHANNEL).unwrap();
        let rows = restored.rows().unwrap();
        let row = rows.row(1).unwrap();
        assert_eq!(row.state, RowState::Held(HeldReason::Ambiguous));
        assert_eq!(row.input["text"], "preserve this input");
        assert_eq!(row.attempts.len(), 1);
        assert!(row.witnesses.is_empty());
    }
}

#[tokio::test]
async fn busy_off_legacy_tracked_retry_keeps_unaccepted() {
    use crate::services::tui_input::attempt::{AttemptMeta, Effect, Tracking};
    use crate::services::tui_input::rows::AttemptEvidence;

    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[(1, "legacy retry")]);
    let anchor = fs::metadata(&world.transcript).unwrap().len();
    let rendered = "[adk:source:1]\nlegacy retry\n[adk:end]";
    let evidence = AttemptEvidence {
        binding: world.binding.clone(),
        execution_nonce: "test-nonce".into(),
        eof: anchor,
        rendered_prompt: rendered.into(),
        source_ids: vec![1],
        record_end: None,
        native_turn_id: None,
    };
    ledger
        .append_tracked(
            1,
            RowState::Injecting,
            Some(evidence),
            &Tracking {
                attempt: Some(AttemptMeta {
                    generation: 1,
                    token: "a".repeat(32),
                    frame_digest: "b".repeat(64),
                    frame_profile: Some("claude-lf-tab4".into()),
                    execution_nonce: "test-nonce".into(),
                    source: world.binding.source.clone(),
                    anchor,
                    effect: Effect::Intent,
                    incarnation: None,
                    queue_end: None,
                }),
                ..Tracking::default()
            },
        )
        .unwrap();
    ledger
        .append_entry(
            &Entry::Transition {
                key: 1,
                state: RowState::Ready,
                attempt: None,
            },
            &[],
        )
        .unwrap();
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.idle()), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    let entered = actor.entered_at();
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.idle()), entered + ACCEPT_WINDOW)
            .await
            .unwrap(),
        Step::Moved(1, RowState::Unaccepted)
    );
    assert_eq!(actor.pane().submitted, [rendered]);
    assert_eq!(ledger.rows().unwrap().row(1).unwrap().attempts.len(), 1);
}
