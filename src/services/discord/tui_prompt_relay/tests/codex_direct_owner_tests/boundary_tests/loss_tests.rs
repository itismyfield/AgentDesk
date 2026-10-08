//! Codex direct answers that end around another source's signal: a Stop hook ahead of the
//! rollout terminal, a turn Codex queued behind a `!cmd` shell turn, a steer into a Discord turn.
use super::*;

const SHELL: &str = "<user_shell_command>\n<command>\nsleep 25; echo SLEPT_6708\n</command>\n\
<result>\nExit code: 0\nOutput:\nSLEPT_6708\n</result>\n</user_shell_command>";

fn event_line(payload: serde_json::Value) -> String {
    rollout_line(serde_json::json!({"type": "event_msg", "payload": payload}))
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
            let first = fx.hook(PROMPT, &t1).await;
            fx.append(&(opening(&t1, PROMPT) + &item_completed_line_for(&t1)));
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let anchor = fx.codex.row().expect("claim").user_msg_id;
            let answer = answer_for(&t1, RESPONSE);
            let (body, terminal) = answer.split_at(answer.find('\n').unwrap() + 1);
            fx.append(body);
            assert!(
                wait_for(Duration::from_secs(10), || tail_starts(fx.codex.channel)
                    == 1)
                .await,
                "no tail"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
            crate::services::claude_tui::hook_server::publish_hook_event_for_tests(HookEvent {
                provider: "codex".to_string(),
                session_id: tmux.to_string(),
                kind: HookEventKind::Stop,
                received_at: chrono::Utc::now(),
                payload: serde_json::json!({"hook_event_name": "Stop", "turn_id": t1}),
                fanout: None,
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
            fx.append(terminal);
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

/// A `!cmd` shell turn ends and Codex starts the input queued behind it: the shell record is no
/// input, and the queued turn's answer reaches its own anchor through one tail.
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
                    + &user_line_for(SHELL)
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
