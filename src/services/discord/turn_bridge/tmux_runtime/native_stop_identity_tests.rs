//! A user stop's Escape targets only the token's own verified native input, judged at `!stop`.
use super::super::super::stop_host::tests::unpinned_generating_turn;
use super::native_contract::running;
use super::*;
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
