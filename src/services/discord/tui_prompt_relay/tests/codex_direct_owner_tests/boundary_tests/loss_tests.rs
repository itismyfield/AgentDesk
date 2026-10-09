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
    fx.append(&(opening(t1, PROMPT) + &item_completed_line_for(t1)));
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
