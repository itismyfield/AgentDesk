//! Every automatic writer obeys the draft protection, each driven through its real entry point.

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;

use super::*;
use crate::services::claude_tui::composer_lock::{
    DraftGuard, DraftRecoveryHold, admit_composer_write, guard_draft,
};
use crate::services::claude_tui::host_input::{
    LegacyTmuxGate, NativeClearSubmission, SpyGuard, SpyState, native_clear_composer_empty,
    native_clear_once,
};
use crate::services::claude_tui::hosting::{
    ClaudeTuiWarmFollowupOutcome, FollowupHost, try_claude_tui_warm_followup,
};
use crate::services::claude_tui::input::{
    PromptReadinessKind, is_prompt_ready_cancelled_error, is_prompt_ready_timeout_error,
    send_followup_prompt_or_idle_transcript, wait_for_prompt_ready, with_composer_cleanup_lock,
};
use crate::services::claude_tui::tui_relay::{SendBackend, router_with_send_backend};
use crate::services::provider::CancelToken;

const BUSY: &str = "\u{2733} Architecting\u{2026}";
const IDLE_HEAD: &str = "\u{23fa} Done.\n\n";
const WORKING_HEAD: &str =
    "\u{23fa} Working on it.\n\n\u{273b} Thinking\u{2026} (12s \u{b7} esc to interrupt)\n";
const FOOTER: &str = "  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)\n";
const READY: &str = "Claude Code v2.1.141\n\n\u{276f} \nstatus";
const RESUME_DIALOG: &str = "\
────────────────────────────────────────────────────────────────────────────────
  This session is 10h 50m old and 367.3k tokens.

  ❯ 1. Resume from summary (recommended)
    2. Resume full session as-is
    3. Don't ask me again

  Enter to confirm · Esc to cancel";

/// An idle transcript holding one recorded turn.
fn idle(tui: &Tui) -> PathBuf {
    let path = tui.dir.path().join("idle.jsonl");
    let turn = r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#;
    fs::write(&path, format!("{turn}\n")).unwrap();
    path
}

fn spy(captures: &[&str], cancel: Option<(&'static str, usize, Arc<CancelToken>)>) -> SpyGuard {
    SpyGuard::install(SpyState {
        captures: captures.iter().map(|c| Some(c.to_string())).collect(),
        cancel_on: cancel,
        ..SpyState::default()
    })
}

/// Pane writes among the spy's calls: keys, typed text, buffer loads, pastes and retires.
fn writes(calls: &[String]) -> Vec<String> {
    let write = |c: &&String| {
        ["keys:", "literal:", "load:", "paste:", "retire:"]
            .iter()
            .any(|k| c.starts_with(k))
    };
    calls.iter().filter(write).cloned().collect()
}

/// The real warm follow-up entry over the spy, reading `pane`; cancelled at its first Enter.
fn warm_follow_up(tui: &Tui, pane: &str) -> (Result<(), String>, Vec<String>) {
    let (session, transcript) = (tui.session(), idle(tui));
    let token = Arc::new(CancelToken::new());
    let mut captures = vec![pane; 12];
    captures.push(BUSY);
    let guard = spy(&captures, Some(("keys:Enter", 1, token.clone())));
    let host = FollowupHost::legacy_tmux(&session);
    let (sender, _stream) = std::sync::mpsc::channel();
    let path = transcript.display().to_string();
    let outcome = try_claude_tui_warm_followup(
        "s".to_string(),
        transcript,
        path,
        true,
        tui.dir.path(),
        "C follow-up",
        sender,
        Some(token),
        &host,
        None,
    );
    let ended = match outcome {
        ClaudeTuiWarmFollowupOutcome::Terminal(result) => result,
        ClaudeTuiWarmFollowupOutcome::Recreate(_) => Err("recreate".to_string()),
    };
    (ended, guard.calls())
}

/// Refused before any pane write, with the error the turn bridge requeues.
fn assert_requeued(ended: &Result<(), String>, calls: &[String]) {
    let error = ended.as_ref().unwrap_err();
    let requeued = is_prompt_ready_timeout_error(error)
        && error.contains("follow-up prompt input readiness")
        && error.contains("reason=draft_recovery_hold")
        && error.contains("prompt_marker_detected=true");
    assert!(requeued, "{error}");
    assert_eq!(writes(calls), Vec::<String>::new());
}

/// `/tui/send` over the fake TUI's own tmux script; `blind` loses every capture.
struct FakeRelay {
    program: PathBuf,
    buffer: PathBuf,
    blind: bool,
}

impl FakeRelay {
    fn tmux(&self, args: &[&str]) -> Result<(), String> {
        let output = std::process::Command::new(&self.program)
            .arg("-u")
            .args(args)
            .output()
            .map_err(|error| error.to_string())?;
        output
            .status
            .success()
            .then_some(())
            .ok_or_else(|| "tmux failed".to_string())
    }
}

impl SendBackend for FakeRelay {
    fn has_session(&self, _session: &str) -> bool {
        true
    }

    fn load_buffer(&self, name: &str, text: &str) -> Result<(), String> {
        fs::write(&self.buffer, text).map_err(|error| error.to_string())?;
        self.tmux(&[
            "load-buffer",
            "-b",
            name,
            &self.buffer.display().to_string(),
        ])
    }

    fn paste_buffer(&self, session: &str, name: &str, _delete: bool) -> Result<(), String> {
        self.tmux(&["paste-buffer", "-p", "-r", "-d", "-b", name, "-t", session])
    }

    fn send_enter(&self, session: &str) -> Result<(), String> {
        self.tmux(&["send-keys", "-t", session, "Enter"])
    }

    fn capture(&self, session: &str) -> Option<String> {
        let pane = Pane::with_program(session, self.program.clone());
        if self.blind { None } else { pane.capture() }
    }
}

/// One `POST /tui/send` with `submit=true` through the real router and handler.
fn relay(tui: &Tui, text: &str, blind: bool) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let backend = FakeRelay {
        program: tui.dir.path().join("tmux"),
        buffer: tui.dir.path().join("relay-buffer"),
        blind,
    };
    let router = router_with_send_backend(Arc::new(backend));
    let body = json!({ "session_name": tui.session(), "text": text, "submit": true });
    let request = axum::http::Request::post("/tui/send")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    })
}

/// The busy inject as deployed: the stash path only on an allowlisted channel.
fn run_as_configured(tui: &Tui) -> Report {
    let session = tui.session();
    let pane = Pane::with_program(&session, tui.dir.path().join("tmux"));
    let request = Request {
        session: &session,
        transcript: &tui.transcript,
        source: "iMessage",
        author: "ann",
        nonce: NONCE,
        text: TEXT,
    };
    inject_report(&pane, &request, &FAST)
}

/// The hold covers the follow-up submit, the cleanup lock and a native `/clear` until a capture
/// shows the stash gone and the composer empty.
#[test]
fn a_held_pane_keeps_the_follow_up_and_native_clear_out_until_recovered() {
    let tui = Tui::new("human draft A");
    tui.put("restore_after", "never");
    let got = tui.run();
    assert_eq!(
        (got, got.delivery()),
        (
            report(Outcome::Injected, DraftState::Unknown),
            Delivery::Observed
        )
    );
    // The turn ends; the draft stays stashed under an empty composer that /clear would accept.
    tui.put("head", IDLE_HEAD);
    let held = tui.capture();
    assert!(native_clear_composer_empty(&held));
    let (session, idle) = (tui.session(), idle(&tui));
    let deadline = || tokio::time::Instant::now() + Duration::from_secs(20);

    let guard = spy(&[&held, &held, &held, BUSY], None);
    let ended = send_followup_prompt_or_idle_transcript(&session, "follow-up", None, &idle);
    let cleanup = with_composer_cleanup_lock(&session, || -> bool { panic!("held pane cleaned") });
    assert_eq!(cleanup, None);
    assert_requeued(&ended, &guard.calls());
    drop(guard);
    let refuse = |_: &[&str], _: Duration| -> bool { panic!("a held pane took /clear") };
    let clear = native_clear_once(
        &session,
        &LegacyTmuxGate,
        deadline(),
        |_| Some(held.clone()),
        refuse,
    );
    assert_eq!(clear, NativeClearSubmission::NotSent);

    // No stash, but the person's text, an attachment chip or an unmeasured footer: still held.
    fs::remove_file(tui.dir.path().join("stash")).unwrap();
    let unreadable = [
        ("human draft A", FOOTER),
        ("[Image #1]", FOOTER),
        ("look at [Image #2]", FOOTER),
        ("[...Truncated text #1 +40 lines...]", FOOTER),
        ("human draft A", "  ? for shortcuts\n"),
        (
            "human draft A",
            "  \u{23f5}\u{23f5} bypass permissions on (shift+tab \n",
        ),
    ];
    for (composer, row) in unreadable {
        tui.put("composer", composer);
        tui.put("footer", row);
        let admitted = admit_composer_write(&session, || Some(tui.capture()));
        assert_eq!(admitted, Err(DraftRecoveryHold), "{composer:?} {row:?}");
    }

    // The person took the draft back and sent it: that capture releases the hold.
    tui.put("composer", "");
    tui.put("footer", FOOTER);
    let recovered = tui.capture();
    let guard = spy(&[&recovered, &recovered, &recovered, BUSY], None);
    let ended = send_followup_prompt_or_idle_transcript(&session, "follow-up", None, &idle);
    assert_eq!(ended, Ok(()));
    assert!(writes(&guard.calls()).contains(&"keys:Enter".to_string()));
    drop(guard);
    let mut sent = 0;
    let clear = native_clear_once(
        &session,
        &LegacyTmuxGate,
        deadline(),
        |_| Some(recovered.clone()),
        |_, _| {
            sent += 1;
            true
        },
    );
    assert_eq!((clear, sent), (NativeClearSubmission::Confirmed, 1));
    for prompt in ["follow-up", "/clear"] {
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            "claude", &session, prompt,
        );
    }
}

/// `/tui/send` sends nothing to a pane in recovery, read or not, and submits once after it.
#[test]
fn the_tui_send_endpoint_keeps_off_a_pane_in_recovery() {
    let tui = Tui::new("human draft A");
    // The Enter is withheld: B, the external input, waits in the composer over A in the stash.
    tui.person_after_capture(3, "attach 300; detach");
    let withheld = Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste);
    assert_eq!(tui.run(), report(withheld, DraftState::Unknown));
    let held = (framed(), Some("human draft A".to_string()));
    for (text, blind) in [("C", false), ("", false), ("C", true), ("", true)] {
        let (status, body) = relay(&tui, text, blind);
        let refused = (StatusCode::CONFLICT, json!("draft_recovery_hold"));
        assert_eq!((status, body["error"].clone()), refused, "{text:?} {blind}");
        assert_eq!(tui.applied(), ["C-s", "paste"], "{text:?} {blind}");
        assert_eq!(tui.drafts(), held, "{text:?} {blind}");
    }
    // The person sent B and dropped A: exactly one submission, holding exactly C.
    tui.put("composer", "");
    fs::remove_file(tui.dir.path().join("stash")).unwrap();
    let (status, body) = relay(&tui, "C", false);
    assert_eq!(
        (status, body["submitted"].clone()),
        (StatusCode::OK, json!(true))
    );
    assert_eq!(tui.applied(), ["C-s", "paste", "paste", "Enter"]);
    assert_eq!(tui.records(), ["C"]);
}

/// A draft Claude handed back stays the person's: the next follow-up and `/tui/send` hold before
/// any write, a busy input stashes it again, and once the person sends it the follow-up goes in.
#[test]
fn a_restored_draft_is_never_sent_with_the_next_follow_up() {
    let tui = Tui::new("human draft A");
    let restored = report(Outcome::Injected, DraftState::RestoredObserved);
    assert_eq!(tui.run(), restored);
    tui.put("head", IDLE_HEAD);
    let (ended, calls) = warm_follow_up(&tui, &tui.capture());
    assert_requeued(&ended, &calls);
    let held = (relay(&tui, "C", false).0, tui.records());
    assert_eq!(held, (StatusCode::CONFLICT, vec![framed()]));
    assert_eq!(tui.drafts(), ("human draft A".to_string(), None));

    tui.put("head", WORKING_HEAD);
    assert_eq!(tui.run_text("again", &FAST), restored);
    let again = frame("iMessage", "ann", NONCE, "again");
    assert_eq!(tui.records(), [framed(), again]);
    assert_eq!(tui.drafts(), ("human draft A".to_string(), None));

    tui.put("composer", "");
    tui.put("head", IDLE_HEAD);
    let (ended, calls) = warm_follow_up(&tui, &tui.capture());
    assert_eq!(ended, Ok(()));
    assert_eq!(writes(&calls), ["literal:C follow-up", "keys:Enter"]);
    crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
        "claude",
        &tui.session(),
        "C follow-up",
    );
}

/// A draft the person brought back from the stash by hand is protected the same way.
#[test]
fn a_draft_brought_back_by_hand_is_never_sent_with_the_next_follow_up() {
    let tui = Tui::new("human draft A");
    tui.put("restore_after", "never");
    assert_eq!(tui.run(), report(Outcome::Injected, DraftState::Unknown));
    for name in ["restore_after", "restore_in", "stash"] {
        fs::remove_file(tui.dir.path().join(name)).unwrap();
    }
    tui.put("composer", "human draft A");
    tui.put("head", IDLE_HEAD);
    let (ended, calls) = warm_follow_up(&tui, &tui.capture());
    assert_requeued(&ended, &calls);
    let held = (relay(&tui, "C", false).0, tui.records());
    assert_eq!(held, (StatusCode::CONFLICT, vec![framed()]));
    assert_eq!(tui.drafts(), ("human draft A".to_string(), None));
}

/// A startup dialog's Enter goes only to a pane protecting no draft, and only if a second look
/// still shows that dialog.
#[test]
fn a_startup_dialog_is_dismissed_only_where_no_draft_is_protected() {
    let dismiss = |session: &str, captures: &[&str]| {
        let token = Arc::new(CancelToken::new());
        let guard = spy(captures, Some(("capture", 6, token.clone())));
        let ready = wait_for_prompt_ready(session, PromptReadinessKind::Followup, Some(&token));
        (ready, writes(&guard.calls()))
    };
    let free = || format!("dialog-free-{}", uuid::Uuid::new_v4());
    // The auth pre-check, the poll and the second look each read the pane once.
    let shown = [RESUME_DIALOG, RESUME_DIALOG, RESUME_DIALOG, READY];
    let (ready, keys) = dismiss(&free(), &shown);
    assert_eq!((ready, keys), (Ok(()), vec!["keys:Enter".to_string()]));
    let gone = [RESUME_DIALOG, RESUME_DIALOG, READY, READY];
    let (ready, keys) = dismiss(&free(), &gone);
    assert_eq!((ready, keys), (Ok(()), Vec::<String>::new()));

    let held = format!("dialog-held-{}", uuid::Uuid::new_v4());
    guard_draft(&held, DraftGuard::RecoveryRequired);
    let (ready, keys) = dismiss(&held, &[RESUME_DIALOG; 10]);
    assert!(ready.is_err_and(|error| is_prompt_ready_cancelled_error(&error)));
    assert_eq!(keys, Vec::<String>::new());
}

/// Only an allowlisted channel's pane takes the stash path; the switch never lifts a protection.
#[test]
fn only_an_allowlisted_channel_takes_the_stash_path() {
    let tui = Tui::new("human draft A");
    let queued = report(Outcome::NotSent(Veto::Draft), DraftState::Unchanged);
    assert_eq!(run_as_configured(&tui), queued);
    assert!(tui.applied().is_empty());
    assert_eq!(tui.drafts(), ("human draft A".to_string(), None));
    assert_eq!(
        tui.run(),
        report(Outcome::Injected, DraftState::RestoredObserved)
    );
    assert_eq!(tui.applied(), ["C-s", "paste", "Enter"]);

    // An empty composer still takes the direct paste, with no C-s.
    let empty = Tui::new("");
    let _ = run_as_configured(&empty);
    assert_eq!(empty.applied().first().map(String::as_str), Some("paste"));
    assert!(!empty.applied().contains(&"C-s".to_string()));

    let held = Tui::new("human draft A");
    held.put("restore_after", "never");
    assert_eq!(held.run(), report(Outcome::Injected, DraftState::Unknown));
    assert_eq!(run_as_configured(&held), queued);
    let admitted = admit_composer_write(&held.session(), || Some(held.capture()));
    assert_eq!(admitted, Err(DraftRecoveryHold));

    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let e2e = 1_509_350_490_461_180_105;
    let listed = stash_channels(Some("1509350490461180105, x ,7"));
    assert_eq!(
        (listed.as_slice(), stash_channels(None)),
        (&[e2e, 7][..], vec![])
    );
    let session = tui.session();
    assert!(!stash_channel_listed(&session, &listed));
    crate::services::tui_prompt_dedupe::register_tmux_channel(&session, e2e);
    assert!(stash_channel_listed(&session, &listed));
    assert!(!stash_channel_listed(&session, &[7]));
}
