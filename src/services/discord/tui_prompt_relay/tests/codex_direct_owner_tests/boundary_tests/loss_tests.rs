//! Codex direct answers that end around another source's signal: a Stop hook ahead of the
//! rollout terminal, a turn Codex queued behind a `!cmd` shell turn, a steer into a Discord turn.
use super::*;

const SHELL: &str = "<user_shell_command>\n<command>\nsleep 25; echo SLEPT_6708\n</command>\n\
<result>\nExit code: 0\nOutput:\nSLEPT_6708\n</result>\n</user_shell_command>";

fn event_line(payload: serde_json::Value) -> String {
    rollout_line(serde_json::json!({"type": "event_msg", "payload": payload}))
}

/// A user record of `turn` as codex-cli 0.160 writes it, its items all of `kind`.
fn user_line_of_kind(text: &str, turn: &str, kind: &str) -> String {
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}],
        "internal_chat_message_metadata_passthrough": {
            "turn_id": turn, "content_item_kinds": [kind]}}}))
}

/// A direct input's turn with its prompt, the tail reading at the rollout end, its anchor.
async fn claimed_turn(fx: &mut Fixture, t1: &str) -> u64 {
    let first = fx.hook(PROMPT, t1).await;
    fx.append(&(model_opening(t1, PROMPT) + &item_completed_line_for(t1)));
    super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
    let anchor = fx.codex.row().expect("claim").user_msg_id;
    fx.append(&rollout_line(
        serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": RESPONSE}]}}),
    ));
    assert!(
        wait_for(Duration::from_secs(10), || tail_starts(fx.codex.channel)
            == 1)
        .await,
        "no tail"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    anchor
}

fn model_opening(turn: &str, prompt: &str) -> String {
    event_line(serde_json::json!({"type": "task_started", "turn_id": turn, "root_turn_id": turn}))
        + &rollout_line(serde_json::json!({"type": "turn_context", "payload": {
            "turn_id": turn, "sandbox_policy": {"type": "danger-full-access"}}}))
        + &user_line_of_kind(prompt, turn, "user.text")
}

/// The Stop hook lands before its turn's `task_complete` record: the answer still reaches its
/// anchor once the record is read, and the turn ends cleanly.
#[test]
fn codex_direct_answer_survives_a_stop_hook_ahead_of_task_complete() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let tmux = "AgentDesk-codex-6708-early-stop";
            let mut fx = Fixture::start(&root, 5_706_810, tmux).await;
            let t1 = turn(1);
            let anchor = claimed_turn(&mut fx, &t1).await;
            publish_stop(tmux, &t1);
            tokio::time::sleep(Duration::from_secs(1)).await;
            fx.append(&event_line(serde_json::json!({"type": "task_complete",
                "turn_id": t1, "last_agent_message": RESPONSE})));
            assert!(
                fx.delivered(RESPONSE).await,
                "answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE, anchor);
            assert_eq!(fx.completed_turns(), 1);
            assert_eq!(tail_starts(fx.codex.channel), 1);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// A Stop hook with no terminal, abort or next turn after it leaves the turn held: no
/// completion and the row stays, however long past the drain; the record then settles it.
#[test]
fn codex_direct_answer_is_held_while_a_stop_hook_has_no_terminal() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let tmux = "AgentDesk-codex-6708-held";
            let mut fx = Fixture::start(&root, 5_706_840, tmux).await;
            let t1 = turn(1);
            let anchor = claimed_turn(&mut fx, &t1).await;
            publish_stop(tmux, &t1);
            tokio::time::sleep(Duration::from_secs(4)).await;
            assert_eq!(fx.completed_turns(), 0, "no completion without a terminal");
            assert_eq!(
                fx.codex.row().map(|row| row.user_msg_id),
                Some(anchor),
                "the turn's row is held"
            );
            fx.append(&event_line(serde_json::json!({"type": "task_complete",
                "turn_id": t1, "last_agent_message": RESPONSE})));
            fx.assert_answers_in_order(&[RESPONSE], Some(1)).await;
            fx.finish();
        })
    });
}

/// A `!cmd` shell turn ends and Codex starts the input queued behind it: the shell's own record
/// is no input, and the queued turn's answer reaches its own anchor through one tail.
#[test]
fn codex_turn_queued_behind_a_shell_turn_answers_on_its_anchor() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_706_820, "AgentDesk-codex-6708-shell").await;
            let mut observed = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
            let (shell, queued) = (turn(1), turn(2));
            fx.append(
                &(event_line(serde_json::json!({"type": "task_started", "turn_id": shell}))
                    + &item_completed_line_for(&shell)
                    + &user_line_of_kind(SHELL, &shell, "shell.user_command")
                    + &event_line(serde_json::json!({"type": "task_complete",
                        "turn_id": shell, "last_agent_message": null}))
                    + &opening(&queued, PROMPT)
                    + &item_completed_line_for(&queued)),
            );
            let event = fx.hook(PROMPT, &queued).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
            fx.append(&answer_for(&queued, RESPONSE));
            fx.assert_answers_in_order(&[RESPONSE], Some(1)).await;
            while let Ok(event) = observed.try_recv() {
                assert!(
                    !event.prompt.contains("SLEPT_6708"),
                    "shell record observed"
                );
            }
            fx.finish();
        })
    });
}

/// A person who types text shaped like a shell record submits an input: it is observed and its
/// answer reaches its own anchor.
#[test]
fn codex_typed_input_shaped_like_a_shell_record_answers_on_its_anchor() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_706_850, "AgentDesk-codex-6708-typed").await;
            let t1 = turn(1);
            let event = fx.hook(SHELL, &t1).await;
            fx.append(
                &(event_line(serde_json::json!({"type": "task_started", "turn_id": t1}))
                    + &rollout_line(
                        serde_json::json!({"type": "turn_context", "payload": {"turn_id": t1}}),
                    )
                    + &user_line_of_kind(SHELL, &t1, "user.text")
                    + &item_completed_line_for(&t1)),
            );
            super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
            fx.append(&answer_for(&t1, RESPONSE));
            fx.assert_answers_in_order(&[RESPONSE], Some(1)).await;
            fx.finish();
        })
    });
}

/// A TUI steer into a Discord-started native turn joins it: its text is echoed once, and no
/// synthetic row, deferred start or tail opens, so the Discord turn alone answers.
#[test]
fn codex_steer_into_a_discord_turn_only_echoes() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let tmux = "AgentDesk-codex-6708-discord";
            let mut fx = Fixture::start(&root, 5_706_830, tmux).await;
            let t1 = turn(1);
            let discord = "[User: probe (ID: 1234567890)] codex discord prompt 6708";
            crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
                "codex", tmux, discord,
            );
            fx.send_hook(discord, &t1);
            fx.append(&(opening(&t1, discord) + &item_completed_line_for(&t1)));
            assert_eq!(fx.steer_hook(PROMPT2, &t1).await, 1, "one echo");
            fx.append(&(user_line_for(PROMPT2) + &answer_for(&t1, RESPONSE)));
            fx.polls(3).await;
            assert_eq!(fx.sent(PROMPT2).len(), 1, "the steer is echoed once");
            assert!(fx.codex.row().is_none(), "no synthetic row");
            assert!(fx.pending_starts().is_empty(), "no deferred start");
            assert_eq!(tail_starts(fx.codex.channel), 0, "no tail");
            fx.finish();
        })
    });
}

fn timestamped(records: &str, at: chrono::DateTime<chrono::Utc>) -> String {
    records
        .lines()
        .map(|line| {
            let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
            record["timestamp"] =
                serde_json::json!(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            rollout_line(record)
        })
        .collect()
}

/// B is observed while A still owns its reader; both native boundaries arrive in one append.
#[test]
fn codex_model_turn_queued_eight_ms_after_completion_delivers_both_answers() {
    run(|root| {
        Box::pin(async move {
            let _pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_845_241, "AgentDesk-codex-5845-model-next").await;
            let (a, b) = (turn(1), turn(2));
            let anchor_a = claimed_turn(&mut fx, &a).await;
            let next = fx.hook(PROMPT2, &b).await;
            assert!(!next.steer_echo);
            assert_eq!(fx.codex.row().unwrap().user_msg_id, anchor_a);
            super::super::super::super::relay_observed_prompt(&fx.shared, next).await;
            let pending = fx.pending_starts();
            assert_eq!(
                pending.len(),
                1,
                "B waits before A is released: {pending:?}"
            );
            let anchor_b = pending[0].1;
            let completed = chrono::Utc::now();
            fx.append(
                &(timestamped(
                    &event_line(serde_json::json!({"type": "task_complete",
            "turn_id": a, "last_agent_message": RESPONSE})),
                    completed,
                ) + &timestamped(
                    &(model_opening(&b, PROMPT2)
                        + &item_completed_line_for(&b)
                        + &answer_for(&b, RESPONSE2)),
                    completed + chrono::Duration::milliseconds(8),
                )),
            );
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], Some(2))
                .await;
            assert_eq!(fx.anchor_of(RESPONSE), anchor_a);
            assert_eq!(fx.anchor_of(RESPONSE2), anchor_b);
            fx.finish();
        })
    });
}

const NEW_TURN_TMUX: &str = r#"#!/bin/sh
d='@D@'
case "$2" in
display-message) echo '0,,80,24';;
capture-pane) if [ -f "$d/pasted" ]; then cat "$d/after"; else cat "$d/before"; fi;;
load-buffer) for last do :; done; cp "$last" "$d/buffer";;
if-shell)
  echo "$7" >> "$d/keys"
  case "$7" in
  paste-buffer*) touch "$d/pasted";;
  send-keys*)
    touch "$d/enter"
    i=0; while [ -f "$d/gate" ] && [ ! -f "$d/release" ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done
    cat "$d/records" >> '@T@';;
  esac;;
esac
"#;

struct NativeLedger {
    session: String,
    target: String,
    nonce: String,
}

impl crate::services::codex_tui::busy_inject::Ledger for NativeLedger {
    fn register(&self) -> bool {
        crate::services::tui_prompt_dedupe::register_injected_steer(
            "codex",
            &self.session,
            &self.nonce,
            &self.target,
            Some(5_845_242),
        )
    }
    fn withdraw(&self) {
        crate::services::tui_prompt_dedupe::withdraw_injected_steer(
            "codex",
            &self.session,
            &self.nonce,
        );
    }
}

/// Enter completes A and records the reserved nonce in B, then all observers join B's delivery.
async fn injected_new_turn_delivers(root: PathBuf, channel: u64, hook_first: bool) {
    use crate::services::codex_tui::busy_inject as inject;
    use std::os::unix::fs::PermissionsExt;
    let _pane = live_pane_tmux(&root);
    let tmux = format!("AgentDesk-codex-5845-injected-{channel}");
    let mut fx = Fixture::start(&root, channel, &tmux).await;
    let (a, b) = (turn(1), turn(2));
    let anchor_a = claimed_turn(&mut fx, &a).await;
    assert_eq!(
        inject::read_turn(&fx.codex.rollout),
        Some(inject::TurnVerdict::KnownModelTurn(a.clone()))
    );
    let nonce = "c4f20001";
    let prompt = crate::services::claude_tui::busy_inject::frame("discord", "ann", nonce, PROMPT2);
    let dir = root.join("inject-pane");
    std::fs::create_dir(&dir).unwrap();
    let program = dir.join("tmux");
    std::fs::write(
        &program,
        NEW_TURN_TMUX
            .replace("@D@", dir.to_str().unwrap())
            .replace("@T@", fx.codex.rollout.to_str().unwrap()),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
    std::fs::write(
        dir.join("before"),
        std::fs::read(fixtures.join("busy_empty.ansi")).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("after"),
        format!(
            "\n› {}\n\n  tab to queue message    95% context left\n",
            prompt.trim_end().replace('\n', "\n  ")
        ),
    )
    .unwrap();
    let completed = chrono::Utc::now();
    std::fs::write(
        dir.join("records"),
        timestamped(
            &event_line(serde_json::json!({
        "type": "task_complete", "turn_id": a, "last_agent_message": RESPONSE})),
            completed,
        ) + &timestamped(
            &(model_opening(&b, &prompt) + &item_completed_line_for(&b)),
            completed + chrono::Duration::milliseconds(8),
        ),
    )
    .unwrap();
    if hook_first {
        std::fs::write(dir.join("gate"), "").unwrap();
    }
    let rollout = fx.codex.rollout.clone();
    let target = a.clone();
    let session = tmux.clone();
    let injected = tokio::task::spawn_blocking(move || {
        let pane = inject::Pane::with_program(&session, program);
        let ledger = NativeLedger {
            session: session.clone(),
            target: target.clone(),
            nonce: nonce.into(),
        };
        inject::inject(
            &pane,
            &inject::Request {
                session: &session,
                rollout: &rollout,
                source: "discord",
                author: "ann",
                nonce,
                text: PROMPT2,
                target_turn: &target,
            },
            &ledger,
            &inject::TIMING,
        )
    });
    let mut next = if hook_first {
        assert!(
            wait_for(Duration::from_secs(10), || dir.join("enter").exists()
                || injected.is_finished())
            .await,
            "injection stalled before Enter"
        );
        if !dir.join("enter").exists() {
            panic!(
                "injection never reached Enter: {:?}",
                injected.await.unwrap()
            );
        }
        let next = fx.hook(&prompt, &b).await;
        assert_eq!(
            fx.codex.row().unwrap().user_msg_id,
            anchor_a,
            "B observed before A cleanup"
        );
        std::fs::write(dir.join("release"), "").unwrap();
        Some(next)
    } else {
        None
    };
    assert_eq!(
        injected.await.unwrap(),
        inject::Outcome::Injected {
            observed_turn: Some(b.clone()),
            joined: false
        }
    );
    if next.is_none() {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), fx.observed.recv())
                .await
                .expect("rollout publishes B")
                .unwrap();
            if event.tmux_session_name == tmux && event.prompt == prompt.trim() {
                next = Some(event);
                break;
            }
        }
        fx.send_hook(&prompt, &b);
    }
    let next = next.unwrap();
    assert!(!next.steer_echo, "B owns its new native turn");
    assert_eq!(next.native_turn_id.as_deref(), Some(b.as_str()));
    super::super::super::super::relay_observed_prompt(&fx.shared, next).await;
    let anchor_b = fx
        .codex
        .row()
        .filter(|row| row.user_msg_id != anchor_a)
        .map(|row| row.user_msg_id)
        .or_else(|| fx.pending_starts().first().map(|record| record.1))
        .expect("B anchor");
    fx.append(&answer_for(&b, RESPONSE2));
    fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], Some(2))
        .await;
    assert_eq!(fx.anchor_of(RESPONSE), anchor_a);
    assert_eq!(fx.anchor_of(RESPONSE2), anchor_b);
    assert_eq!(
        fx.relay_echoes(prompt.trim()).await,
        0,
        "no duplicate observation or echo"
    );
    let keys = std::fs::read_to_string(dir.join("keys")).unwrap();
    assert_eq!(keys.lines().count(), 2, "one paste and one Enter");
    fx.finish();
}

#[test]
fn codex_injected_new_turn_hook_first_delivers_its_answer() {
    run(|root| Box::pin(injected_new_turn_delivers(root, 5_845_242, true)));
}

#[test]
fn codex_injected_new_turn_rollout_first_delivers_its_answer() {
    run(|root| Box::pin(injected_new_turn_delivers(root, 5_845_243, false)));
}
