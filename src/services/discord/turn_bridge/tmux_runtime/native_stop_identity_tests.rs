//! A user stop's Escape targets only the token's own verified native input, judged at `!stop`.
use super::super::super::NativeStop;
use super::super::super::stop_host::tests::unpinned_generating_turn;
use super::native_contract::running;
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use std::io::Write;

fn append(path: &std::path::Path, line: serde_json::Value) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

/// A human queued prompt as Claude records a busy paste it absorbs mid-turn.
fn queued(uuid: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({"type":"attachment","uuid":uuid,"isSidechain":false,
        "attachment":{"type":"queued_command","prompt":prompt,"commandMode":"prompt",
        "origin":{"kind":"human"},"humanTurn":true}})
}

/// Registers a busy frame in the ledger as the owner's busy paste does, before its row lands.
fn ledger(session: &str, attempt: &str, path: &std::path::Path, prompt: &str) -> String {
    let frame =
        crate::services::claude_tui::busy_inject::frame("discord", "7", "owner", attempt, prompt);
    let offset = std::fs::metadata(path).unwrap().len();
    crate::services::tui_input::human_frame::history::register(
        ("claude", session, attempt, "7"),
        path,
        offset,
        &frame,
    )
    .unwrap();
    frame
}

fn escapes(fx: &Fixture) -> usize {
    let calls = fx.take_calls();
    let escape = |call: &&String| call.starts_with("send-keys") && call.ends_with("Escape");
    calls.iter().filter(escape).count()
}

/// Neutral and own busy rows after the pinned input keep the Escape; any other prompt, a missing
/// pin or a changed pane or file sends none and never falls back to a raw cancel.
#[test]
fn a_stop_escapes_only_the_turn_its_own_input_still_owns() {
    let fx = Fixture::new();
    run(async {
        for (n, (case, sent)) in [
            ("neutral-rows", 1),
            ("own-busy-input", 1),
            ("long-turn", 1),
            ("later-user-prompt", 0),
            ("unregistered-human-prompt", 0),
            ("busy-input-of-another-token", 0),
            ("busy-input-of-another-session", 0),
            ("no-pin", 0),
            ("respawned-pane", 0),
            ("replaced-file", 0),
        ]
        .into_iter()
        .enumerate()
        {
            fx.serve(Server::Live);
            fx.pane(&running());
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_212_300 + n as u64);
            let session = format!("native-identity-{case}");
            let (token, path) = match case {
                "no-pin" => unpinned_generating_turn(&fx, &session),
                _ => generating_turn(&fx, &session),
            };
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let tool = serde_json::json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"tool-d","name":"Bash","input":{"command":"sleep 60"}}]}});
            append(&path, tool);
            match case {
                "neutral-rows" => {
                    append(
                        &path,
                        serde_json::json!({"type":"user","isMeta":true,"message":{"content":"meta"}}),
                    );
                    append(
                        &path,
                        serde_json::json!({"type":"user","isSidechain":true,"uuid":"side",
                            "message":{"role":"user","content":"subagent prompt"}}),
                    );
                    let mut notice =
                        queued("notice", "<task-notification>done</task-notification>");
                    notice["attachment"]["commandMode"] = "task-notification".into();
                    append(&path, notice);
                }
                "own-busy-input" => {
                    let frame = ledger(&session, "busy-own", &path, "follow up");
                    token.native_stop.register_busy_child("busy-own");
                    append(&path, queued("row-own", &frame));
                }
                "long-turn" => append(
                    &path,
                    serde_json::json!({"type":"assistant","message":{"content":[
                        {"type":"text","text":"x".repeat(300_000)}]}}),
                ),
                "later-user-prompt" => append(
                    &path,
                    serde_json::json!({"type":"user","uuid":"later","sessionId":"fixture-sid",
                        "message":{"role":"user","content":"typed into the pane"}}),
                ),
                "unregistered-human-prompt" => append(&path, queued("row-typed", "typed")),
                "busy-input-of-another-token" => {
                    let frame = ledger(&session, "busy-other", &path, "follow up");
                    append(&path, queued("row-other", &frame));
                }
                "busy-input-of-another-session" => {
                    let frame = ledger("another-session", "busy-elsewhere", &path, "follow up");
                    token.native_stop.register_busy_child("busy-elsewhere");
                    append(&path, queued("row-elsewhere", &frame));
                }
                "respawned-pane" => std::fs::write(
                    crate::services::tmux_common::session_temp_path(&session, "spawn_nonce"),
                    "another-pane",
                )
                .unwrap(),
                "replaced-file" => {
                    let replacement = path.with_extension("replacement");
                    std::fs::copy(&path, &replacement).unwrap();
                    std::fs::rename(replacement, &path).unwrap();
                }
                _ => {}
            }
            // Claude keeps generating after each judged row, as the running pane shows.
            append(
                &path,
                serde_json::json!({"type":"assistant","message":{"content":[
                    {"type":"tool_use","id":"tool-e","name":"Bash","input":{"command":"sleep 60"}}]}}),
            );
            let _ = fx.take_calls();
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            assert!(matches!(result, CommandStop::Native(_)), "{case}");
            assert_eq!(escapes(&fx), sent, "{case}");
            assert!(
                !token.cancelled.load(Ordering::Acquire),
                "{case}: no raw cancel"
            );
            assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
            assert_eq!(tombstone(channel), None, "{case}");
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

/// Rows that land while the stop waits on the composer are judged again under the lock: another
/// prompt there sends no Escape, and the token's own busy input there keeps it.
#[test]
fn rows_landing_while_the_stop_waits_on_the_composer_are_judged_again() {
    let fx = Fixture::new();
    run(async {
        for (n, (case, sent)) in [("own-busy-input", 1), ("unregistered-human-prompt", 0)]
            .into_iter()
            .enumerate()
        {
            fx.serve(Server::Live);
            fx.pane(&running());
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_212_320 + n as u64);
            let session = format!("native-identity-lock-{case}");
            let (token, path) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let (holding, held) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let locked = session.clone();
            let lock = std::thread::spawn(move || {
                crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                    &locked,
                    || {
                        holding.send(()).unwrap();
                        released.recv().unwrap();
                    },
                );
            });
            held.recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let _ = fx.take_calls();
            let request = begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop");
            let land = async {
                tokio::time::timeout(std::time::Duration::from_secs(8), async {
                    while !fx
                        .calls()
                        .iter()
                        .any(|call| call.starts_with("capture-pane"))
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("the stop reads the pane before waiting on the composer");
                // The probe's own check ends well inside this, so the row lands under the wait.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                match case {
                    "own-busy-input" => {
                        let frame = ledger(&session, "busy-late", &path, "late follow up");
                        token.native_stop.register_busy_child("busy-late");
                        append(&path, queued("row-late", &frame));
                    }
                    _ => append(&path, queued("row-late-typed", "typed while waiting")),
                }
                release.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(request, land);
            lock.join().unwrap();
            assert!(matches!(result, CommandStop::Native(_)), "{case}");
            assert_eq!(escapes(&fx), sent, "{case}");
            assert!(!token.cancelled.load(Ordering::Acquire), "{case}");
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

fn tool_use(id: &str) -> serde_json::Value {
    serde_json::json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":id,"name":"Bash","input":{"command":"sleep 60"}}]}})
}

/// An acknowledged Escape alone publishes no stop; the pinned turn's own end does.
async fn stops_at_its_own_end(stop: &NativeStop, path: &std::path::Path, case: &str) {
    let early = tokio::time::timeout(std::time::Duration::from_millis(200), stop.finish()).await;
    assert!(
        early.is_err(),
        "{case}: an acknowledged Escape publishes nothing"
    );
    append(
        path,
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
    );
    assert!(
        stop.finish().await.token.is_some(),
        "{case}: the turn's end publishes the stop"
    );
}

/// Hook context beside a tool result belongs to the running turn, so its stop still escapes it;
/// the same text alone or a later prompt opens another input and sends none.
#[test]
fn hook_context_beside_a_tool_result_keeps_the_turn_its_own() {
    let fx = Fixture::new();
    run(async {
        let hook = "<system-reminder>hook additional context</system-reminder>";
        let cases = [
            ("beside-tool-result", 1),
            ("alone", 0),
            ("then-user-input", 0),
        ];
        for (n, (case, sent)) in cases.into_iter().enumerate() {
            fx.serve(Server::Live);
            fx.pane(&running());
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_212_340 + n as u64);
            let session = format!("native-identity-hook-{case}");
            let (token, path) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            append(&path, tool_use("tool-h"));
            let text = serde_json::json!({"type":"text","text":hook});
            let content = match case {
                "alone" => serde_json::json!([text]),
                _ => {
                    serde_json::json!([{"type":"tool_result","tool_use_id":"tool-h","content":"ok"}, text])
                }
            };
            append(
                &path,
                serde_json::json!({"type":"user","uuid":"hooked","sessionId":"fixture-sid",
                    "message":{"role":"user","content":content}}),
            );
            if case == "then-user-input" {
                append(
                    &path,
                    serde_json::json!({"type":"user","uuid":"next","sessionId":"fixture-sid",
                        "message":{"role":"user","content":"a later prompt"}}),
                );
            }
            append(&path, tool_use("tool-i"));
            let _ = fx.take_calls();
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            let CommandStop::Native(stop) = result else {
                panic!("{case}: a native stop");
            };
            assert_eq!(escapes(&fx), sent, "{case}");
            assert!(!token.cancelled.load(Ordering::Acquire), "{case}");
            assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
            assert_eq!(tombstone(channel), None, "{case}");
            match sent {
                1 => stops_at_its_own_end(&stop, &path, case).await,
                _ => assert!(stop.finish().await.token.is_none(), "{case}"),
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

fn bind_pane(session: &str, path: &std::path::Path) {
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
}

/// A recovered token keeps its episode's busy inputs whether the binding returns before or after
/// restore; another episode's, session's or an unrecorded busy row still sends no Escape.
#[test]
fn a_recovered_turn_escapes_past_only_its_own_busy_inputs() {
    let fx = Fixture::new();
    run(async {
        let cases = [
            ("own", true, 1),
            ("own", false, 1),
            ("other-parent", true, 0),
            ("other-source", true, 0),
            ("no-parent", false, 0),
        ];
        for (n, (case, bound_first, sent)) in cases.into_iter().enumerate() {
            fx.serve(Server::Live);
            fx.pane(&running());
            let channel = ChannelId::new(7_212_360 + n as u64);
            let session = format!("native-identity-restart-{n}");
            let (old, path) = generating_turn(&fx, &session);
            let row_path = native_inflight_row(channel, &old, &session);
            let load =
                || inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
            let mut row = load().unwrap();
            row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            row.output_path = Some(path.display().to_string());
            inflight::save_inflight_state(&row).unwrap();
            // The live bridge persists the token's verified pin before any busy input.
            inflight::bind_native_stop_pin(&row, &old);
            append(&path, tool_use("tool-r"));
            let row = load().unwrap();
            let (attempt, source) = match case {
                "other-source" => ("busy-elsewhere", "another-session"),
                _ => ("busy-b", session.as_str()),
            };
            let frame = ledger(source, attempt, &path, "follow up");
            if case != "no-parent" {
                inflight::record_native_busy_child(&row, &old, attempt);
            }
            append(&path, queued("row-b", &frame));
            append(&path, tool_use("tool-s"));
            let mut raw = serde_json::to_value(load().unwrap()).unwrap();
            assert!(
                raw["native_stop_user_pin"].is_object(),
                "{case}: the pin is durable"
            );
            if case == "other-parent" {
                raw["native_stop_busy_children"]["turn_nonce"] = "another-turn".into();
                std::fs::write(&row_path, raw.to_string()).unwrap();
            }
            drop(old);
            // A restarted app: a fresh token for the persisted nonce, rehydrated from the row.
            let shared = crate::services::discord::make_shared_data_for_tests();
            let token = Arc::new(CancelToken::from_persisted_turn_nonce(
                row.turn_nonce.clone(),
            ));
            token.bind_claude_tmux_session(&session);
            start(&shared, channel, &token).await;
            if !bound_first {
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
            }
            let before = file_state(&row_path);
            inflight::restore_native_stop_pin(&load().unwrap(), &token);
            assert_eq!(
                file_state(&row_path),
                before,
                "{case}: restore writes nothing durable"
            );
            if !bound_first {
                bind_pane(&session, &path);
            }
            let _ = fx.take_calls();
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            let CommandStop::Native(stop) = result else {
                panic!("{case}: a native stop");
            };
            assert_eq!(escapes(&fx), sent, "{case} bound_first={bound_first}");
            assert!(!token.cancelled.load(Ordering::Acquire), "{case}");
            assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
            assert_eq!(tombstone(channel), None, "{case}");
            match sent {
                1 => stops_at_its_own_end(&stop, &path, case).await,
                _ => assert!(stop.finish().await.token.is_none(), "{case}"),
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}
