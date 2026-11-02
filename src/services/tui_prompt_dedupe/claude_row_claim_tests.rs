//! Who answers a Claude input pasted through a real entry, read from the row it produced: every
//! cell pastes once into a scripted pane, writes Claude's rows and runs the relay's observers.
use super::*;
use crate::services::claude_tui::busy_inject::{self, FAKE_STASH_TUI, Pane, Request, Timing};
use std::os::unix::fs::PermissionsExt;

const TIMING: Timing = Timing {
    lock_retries: &[
        Duration::ZERO,
        Duration::from_millis(5),
        Duration::from_millis(10),
    ],
    settle: Duration::ZERO,
    confirm_window: Duration::from_secs(2),
    confirm_poll: Duration::from_millis(10),
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    /// Claude took the queued prompt into the running turn.
    Attachment,
    /// An idle Claude opened a turn with it while the receipt window was open.
    UserInWindow,
    /// A long turn ended after the window closed and the queued prompt opened the next turn.
    UserAfterLongTurn,
    EnqueueOnly,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    /// The owner's running-pane entry, which declares no answerer.
    Busy,
    /// An entry whose caller declares the prompt pending and reads the turn itself.
    Declared,
}

struct Cell {
    published: usize,
    observations: Vec<PromptObservation>,
    keys: Vec<String>,
    pending_left: bool,
}

const BUSY_TURN: &str = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
    {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";

/// A scripted busy Claude pane writing `transcript.jsonl` in its own directory.
fn fake_pane() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/busy-inject-tmp");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let path = |name: &str| dir.path().join(name);
    let transcript = path("transcript.jsonl");
    std::fs::write(&transcript, BUSY_TURN).unwrap();
    let script = FAKE_STASH_TUI
        .replace("@D@", &dir.path().display().to_string())
        .replace("@T@", &transcript.display().to_string());
    std::fs::write(path("tmux"), script).unwrap();
    std::fs::set_permissions(path("tmux"), std::fs::Permissions::from_mode(0o755)).unwrap();
    for (name, value) in [
        ("attached", "0".to_string()),
        ("last", "100".to_string()),
        ("width", "160".to_string()),
        ("height", "50".to_string()),
        (
            "head",
            "⏺ Working.\n\n✻ Thinking… (3s · esc to interrupt)\n".into(),
        ),
        ("border", format!("{}\n", "─".repeat(60))),
        ("prompt", "❯\u{00a0}".into()),
        ("footer", "  ⏵⏵ bypass permissions on\n".into()),
        ("composer", String::new()),
    ] {
        std::fs::write(path(name), value).unwrap();
    }
    dir
}

fn run_cell(turn_mode: bool, row: Row, route: Route, at: u64) -> Cell {
    let dir = fake_pane();
    let path = |name: &str| dir.path().join(name);
    let transcript = path("transcript.jsonl");
    // The row a cell names follows from the turn Claude reads: idle for a direct user row.
    let idle_turn = format!("{BUSY_TURN}{{\"type\":\"system\",\"subtype\":\"turn_duration\"}}\n");
    let before = if row == Row::UserInWindow {
        idle_turn.as_str()
    } else {
        BUSY_TURN
    };
    std::fs::write(&transcript, before).unwrap();
    let session = format!("row-claim-{at}");
    let channel = 7_676_200 + at;
    register_tmux_channel(&session, channel);
    let _turns =
        turn_mode.then(|| crate::services::tui_o::turn_mode::TestConfirmation::new(channel));
    let nonce = busy_inject::fresh_nonce();
    let text = format!("row claim question {at}");
    let request = Request {
        session: &session,
        transcript: &transcript,
        source: "discord",
        author: "200",
        name: "ann",
        nonce: &nonce,
        text: &text,
    };
    let frame = busy_inject::frame("discord", "200", "ann", &nonce, &text);
    let offset = std::fs::metadata(&transcript).unwrap().len();
    let pane = Pane::with_program(&session, path("tmux"));
    let report = match route {
        Route::Busy => busy_inject::inject_owned(&pane, &request, &TIMING),
        Route::Declared => busy_inject::inject_report_any_turn(&pane, &request, None, &TIMING),
    };
    assert_eq!(
        report.outcome,
        busy_inject::Outcome::Injected,
        "{row:?} {route:?}"
    );
    let keys: Vec<String> = std::fs::read_to_string(path("applied"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let user = serde_json::json!({"type":"user","uuid":format!("user-{at}"),
        "message":{"role":"user","content":frame}});
    let append = |record: serde_json::Value| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    };
    if row == Row::UserInWindow {
        // An idle Claude writes the user row with no enqueue in front of it.
        std::fs::write(&transcript, before).unwrap();
        append(user.clone());
    }
    // The receipt thread's window: it records the first row that carries the frame.
    let receipt = (transcript.as_path(), offset, nonce.as_str(), frame.as_str());
    let deadline = std::time::Instant::now() + TIMING.confirm_window;
    let _ = busy_inject::await_receipt(receipt, deadline, TIMING.confirm_poll);
    let attachment = serde_json::json!({"type":"attachment","uuid":format!("attachment-{at}"),
        "attachment":{"type":"queued_command","prompt":frame,"commandMode":"prompt"}});
    match row {
        Row::Attachment => append(attachment),
        Row::UserAfterLongTurn => {
            append(serde_json::json!({"type":"system","subtype":"turn_duration"}));
            append(user);
        }
        Row::UserInWindow | Row::EnqueueOnly => {}
    }
    let mut events = subscribe_observed_prompts();
    let now = Utc::now();
    let mut observations = vec![observe_hook_prompt_by_tmux_with_prompt_id_at(
        "claude", &session, &frame, None, now,
    )];
    let rows = match row {
        Row::Attachment => vec![format!("attachment-{at}")],
        Row::UserInWindow | Row::UserAfterLongTurn => vec![format!("user-{at}")],
        Row::EnqueueOnly => vec![],
    };
    // The scanner meets each row twice: the second reading is a rescan.
    for row in rows.iter().chain(rows.iter()) {
        observations.push(observe_prompt_by_tmux_with_row_ids_at(
            "claude",
            &session,
            &frame,
            Some(row),
            None,
            now,
        ));
    }
    let mut published = 0;
    while let Ok(event) = events.try_recv() {
        published += usize::from(event.tmux_session_name == session && event.prompt == frame);
    }
    let pending_left = {
        let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        state
            .pending_by_tmux
            .get(&PromptKey::new("claude", &session))
            .is_some_and(|queue| queue.iter().any(|p| p.value == frame))
    };
    Cell {
        published,
        observations,
        keys,
        pending_left,
    }
}

/// Channel (turn mode, legacy) x row x entry: the legacy relay owns exactly the input whose user
/// row opened a turn and that no caller declared; nothing else, and no rescan, publishes it.
#[test]
fn claude_input_answerer_is_read_from_its_row() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let rows = [
        Row::Attachment,
        Row::UserInWindow,
        Row::UserAfterLongTurn,
        Row::EnqueueOnly,
    ];
    let mut cells = 0;
    let mut at = 0;
    for turn_mode in [true, false] {
        for row in rows {
            for route in [Route::Busy, Route::Declared] {
                at += 1;
                let cell = run_cell(turn_mode, row, route, at);
                let context = format!(
                    "turn_mode={turn_mode} {row:?} {route:?}: {:?}",
                    cell.observations
                );
                assert_eq!(cell.keys, ["paste", "Enter"], "{context}");
                let opened = matches!(row, Row::UserInWindow | Row::UserAfterLongTurn);
                let legacy_relay = !turn_mode && opened && route == Route::Busy;
                assert_eq!(cell.published, usize::from(legacy_relay), "{context}");
                // Only a caller that reads the turn itself declares it pending.
                let declared_reader = !turn_mode && opened && route == Route::Declared;
                if declared_reader {
                    assert!(
                        cell.observations
                            .contains(&PromptObservation::SuppressedDiscordDuplicate),
                        "{context}"
                    );
                }
                assert_eq!(
                    cell.pending_left,
                    route == Route::Declared && !declared_reader,
                    "{context}"
                );
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 16);
}

/// The owner's entry sends no key on a screen it cannot prove submits: an unread capture or copy
/// mode is a typed refusal the owner holds, while the same pane read normally takes the input.
#[test]
fn owned_entry_sends_no_key_on_a_screen_it_cannot_prove_submits() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for (at, screen, expected) in [
        (
            1,
            "fail.cap.1",
            busy_inject::Outcome::NotSent(busy_inject::Veto::ScreenUnread),
        ),
        (
            2,
            "in_mode",
            busy_inject::Outcome::NotSent(busy_inject::Veto::CopyMode),
        ),
        (3, "", busy_inject::Outcome::Injected),
    ] {
        let dir = fake_pane();
        if !screen.is_empty() {
            std::fs::write(dir.path().join(screen), "1").unwrap();
        }
        let (session, nonce) = (format!("screen-hold-{at}"), busy_inject::fresh_nonce());
        let transcript = dir.path().join("transcript.jsonl");
        let request = Request {
            session: &session,
            transcript: &transcript,
            source: "discord",
            author: "200",
            name: "ann",
            nonce: &nonce,
            text: "held until the screen submits",
        };
        let pane = Pane::with_program(&session, dir.path().join("tmux"));
        let report = busy_inject::inject_owned(&pane, &request, &TIMING);
        assert_eq!(report.outcome, expected, "{screen}");
        let keys = std::fs::read_to_string(dir.path().join("applied")).unwrap_or_default();
        let sent = usize::from(screen.is_empty()) * 2;
        assert_eq!(keys.lines().count(), sent, "{screen}: {keys}");
    }
}
