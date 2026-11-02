use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude::first_input::{Submitted, submit};
use crate::services::claude_tui::guarded_outcome::SubmissionError;
use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view};
use crate::services::claude_tui::{
    guarded_outcome,
    host_input::{SpyGuard, SpyState},
};
use crate::services::provider::CancelToken;
use crate::services::provider::native_stop::NativeUserPin;
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
use serde_json::json;
use std::sync::Arc;

const IDLE: &str = include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
fn user(text: &str, id: &str) -> String {
    json!({"type":"user","uuid":id,"sessionId":"receipt-sid","message":{"role":"user","content":text}}).to_string()
}
fn binding(session: &str, path: &std::path::Path) {
    tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("receipt-sid".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
}
fn ready() -> SpyGuard {
    SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(IDLE.into()), 64).collect(),
        ..Default::default()
    })
}
fn fresh(
    session: &str,
    path: &std::path::Path,
    prompt: &str,
    token: &CancelToken,
) -> Result<Submitted, SubmissionError> {
    guarded_outcome::with_owner(None, || {
        guarded_outcome::fresh_receipt(session, prompt, &path.display().to_string(), "receipt-sid");
        submit(&path.display().to_string(), 0, Some(token), session, prompt)
    })
}
fn observed(token: &Arc<CancelToken>, path: &std::path::Path) -> Option<Result<NativeUserPin, ()>> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}\n")
        .unwrap();
    let (sender, _receiver) = std::sync::mpsc::channel();
    let result =
        crate::services::session_backend::read_claude_tui_output_file_until_result_with_harvest(
            &path.display().to_string(),
            0,
            sender,
            Some(token.clone()),
            crate::services::provider::SessionProbe::process(|| true),
        )
        .unwrap();
    assert!(matches!(
        result.0,
        crate::services::provider::ReadOutputResult::Completed { .. }
    ));
    token.native_stop.receipt_sink().observed()
}

#[test]
fn b1_fresh_writer_pins_after_real_boundary_for_existing_and_enoent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    for exists in [true, false] {
        tmux.clear();
        let session = format!("b1-fresh-{exists}");
        let path = dir.path().join(format!("{exists}.jsonl"));
        let past = format!("{}\n", user("own", "past"));
        if exists {
            std::fs::write(&path, &past).unwrap();
        }
        binding(&session, &path);
        tmux.on_enter(&path, &user("own", "first-a"));
        let token = Arc::new(CancelToken::new());
        fresh(&session, &path, "own", &token).unwrap();
        assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
        let pin = observed(&token, &path)
            .expect("Applied writer must reach token sink")
            .unwrap();
        assert_eq!(pin.uuid, "first-a");
        assert_eq!(pin.range.0, if exists { past.len() as u64 } else { 0 });
        assert!(!token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}

#[test]
fn b2_idle_leaf_collects_for_turn_without_enabling_cancel_gate() {
    use crate::services::discord::queue_io::input_owner::effect::{Completion, idle_claude};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idle.jsonl");
    let past = format!("{}\n", user("own", "past"));
    std::fs::write(&path, &past).unwrap();
    let session = "b2-idle";
    binding(session, &path);
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    tmux.on_enter(&path, &user("own", "idle-a"));
    let token = Arc::new(CancelToken::new());
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        idle_claude(session.into(), token.clone())("own".into()),
        Ok(Completion::Submitted)
    );
    assert_eq!(
        tmux.sends(),
        ["load", "paste", "Enter"],
        "B keeps idle cancel=None"
    );
    token
        .cancelled
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let pin = observed(&token, &path)
        .expect("idle receipt belongs to its turn")
        .unwrap();
    assert_eq!(pin.uuid, "idle-a");
    assert_eq!(pin.range.0, past.len() as u64);
    assert_eq!(
        CancelToken::new().native_stop.receipt_sink().observed(),
        None,
        "a restart/restore token cannot inherit writer evidence"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn b3_first_foreign_user_is_irrevocable_even_when_exact_arrives_later() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.jsonl");
    let session = "b3-foreign";
    binding(session, &path);
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    tmux.on_enter(
        &path,
        &format!("{}\n{}", user("foreign", "b"), user("own", "a")),
    );
    let token = Arc::new(CancelToken::new());
    fresh(session, &path, "own", &token).unwrap();
    assert_eq!(
        observed(&token, &path),
        Some(Err(())),
        "first foreign candidate must be rejected, never skipped"
    );
    std::fs::write(&path, format!("{}\n", user("own", "later-a"))).unwrap();
    assert_eq!(
        observed(&token, &path),
        Some(Err(())),
        "rejection is immutable"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn b4_failed_enter_never_activates_receipt_or_replays() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("failed.jsonl");
    let session = "b4-enter";
    binding(session, &path);
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    tmux.fail_send(2);
    let token = Arc::new(CancelToken::new());
    assert!(fresh(session, &path, "own", &token).is_err());
    std::fs::write(&path, format!("{}\n", user("own", "unproven"))).unwrap();
    assert_eq!(
        observed(&token, &path),
        Some(Err(())),
        "failed Enter cannot create an Applied pin"
    );
    assert_eq!(
        tmux.sends(),
        ["load", "paste", "Enter"],
        "no replay after uncertain write"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn b5_crlf_wrapper_is_exact_but_replacement_inode_is_rejected() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    for replace in [false, true] {
        tmux.clear();
        let session = format!("b5-exact-{replace}");
        let path = dir.path().join(format!("{replace}.jsonl"));
        let frame = format!("head\r\n{}\r\ntail", "긴 입력 ".repeat(500));
        std::fs::write(&path, format!("{}\n", user(&frame, "past"))).unwrap();
        binding(&session, &path);
        tmux.on_enter(
            &path,
            &user(
                &format!(
                    "\n\n<pasted_content id=\"a1b2\">\n{}\n</pasted_content id=\"a1b2\">\n",
                    frame.replace("\r\n", "\n")
                ),
                "exact-a",
            ),
        );
        let token = Arc::new(CancelToken::new());
        fresh(&session, &path, &frame, &token).unwrap();
        if replace {
            let replacement = path.with_extension("new");
            std::fs::copy(&path, &replacement).unwrap();
            std::fs::rename(replacement, &path).unwrap();
            assert_eq!(
                observed(&token, &path),
                Some(Err(())),
                "other inode is never receipt proof"
            );
        } else {
            assert_eq!(
                observed(&token, &path)
                    .expect("exact frame receipt")
                    .unwrap()
                    .uuid,
                "exact-a"
            );
        }
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}

#[test]
fn b6_reader_observes_native_user_appended_after_applied_and_seal() {
    use std::io::Write;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late-native.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = "b6-reader-late";
    binding(session, &path);
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    let token = Arc::new(CancelToken::new());
    fresh(session, &path, "own", &token).unwrap();
    assert_eq!(
        token.native_stop.receipt_sink().observed(),
        None,
        "seal is not a UUID"
    );
    let (started, receive_started) = std::sync::mpsc::sync_channel(1);
    let (sender, _receiver) = std::sync::mpsc::channel();
    let (reader_token, reader_path) = (token.clone(), path.clone());
    let reader = std::thread::spawn(move || {
        crate::services::session_backend::read_claude_tui_output_file_until_result_with_harvest(
            &reader_path.display().to_string(),
            0,
            sender,
            Some(reader_token),
            crate::services::provider::SessionProbe::process(move || {
                let _ = started.try_send(());
                true
            }),
        )
        .unwrap()
    });
    receive_started
        .recv_timeout(std::time::Duration::from_secs(4))
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", user("own", "late-a")).unwrap();
    writeln!(
        file,
        "{{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}}"
    )
    .unwrap();
    assert!(matches!(
        reader.join().unwrap().0,
        crate::services::provider::ReadOutputResult::Completed { .. }
    ));
    assert_eq!(
        token
            .native_stop
            .receipt_sink()
            .observed()
            .unwrap()
            .unwrap()
            .uuid,
        "late-a"
    );
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn b7_sid_spawn_partial_and_missing_uuid_never_borrow_a_later_user() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    for case in ["sid", "spawn", "uuid", "fuzzy", "partial"] {
        tmux.clear();
        let session = format!("b7-{case}");
        let path = dir.path().join(format!("{case}.jsonl"));
        std::fs::write(&path, "").unwrap();
        binding(&session, &path);
        let mut first: serde_json::Value = serde_json::from_str(&user("own", "a")).unwrap();
        match case {
            "sid" => first["sessionId"] = json!("foreign-sid"),
            "uuid" => {
                first.as_object_mut().unwrap().remove("uuid");
            }
            "fuzzy" => first["message"]["content"] = json!("prefix own suffix"),
            _ => {}
        }
        if case != "partial" {
            tmux.on_enter(&path, &first.to_string());
        }
        let token = Arc::new(CancelToken::new());
        fresh(&session, &path, "own", &token).unwrap();
        if case == "spawn" {
            let marker = crate::services::tmux_common::session_temp_path(&session, "spawn_nonce");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, "new-spawn").unwrap();
        }
        if case == "partial" {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"{\"type\":\"user\"")
                .unwrap();
            assert_eq!(
                token.native_stop.observe_native_input(),
                None,
                "partial line is not a user proof"
            );
        } else {
            assert_eq!(observed(&token, &path), Some(Err(())), "{case}");
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap(),
                "{}",
                user("own", "later")
            )
            .unwrap();
            assert_eq!(
                observed(&token, &path),
                Some(Err(())),
                "{case} rejection cannot change"
            );
        }
        assert_eq!(
            CancelToken::new().native_stop.receipt_sink().observed(),
            None,
            "restore/direct token stays without writer proof"
        );
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}

#[test]
fn b8_managed_receipt_and_token_share_one_writer_snapshot() {
    use crate::services::discord::inflight::{
        InflightTurnState, install_managed_submission_boundary, save_inflight_state_create_new,
    };
    use crate::services::{
        claude_tui::submission_fence, provider::ProviderKind, turn_host::TurnHost,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = "b8-managed-writer";
    binding(session, &path);
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    tmux.on_enter(&path, &user("own", "managed-a"));
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        7_212_808,
        Some("receipt-sid".into()),
        7,
        11,
        12,
        "own".into(),
        None,
        Some(session.into()),
        None,
        None,
        0,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    let managed = install_managed_submission_boundary(&mut row, &TurnHost::Tmux).unwrap();
    save_inflight_state_create_new(&row).unwrap();
    let token = Arc::new(CancelToken::new());
    guarded_outcome::with_owner(Some(managed.observer()), || {
        submission_fence::with_scope(Some(managed.fence()), || {
            guarded_outcome::fresh_receipt(
                session,
                "own",
                &path.display().to_string(),
                "receipt-sid",
            );
            let _seal = guarded_outcome::seal_on_exit(session);
            submit(&path.display().to_string(), 0, Some(&token), session, "own").unwrap();
        });
    });
    assert!(
        managed.exact_user_receipt(),
        "Native collection preserves the existing owner's receipt"
    );
    assert_eq!(observed(&token, &path).unwrap().unwrap().uuid, "managed-a");
    assert!(
        managed.exact_user_receipt(),
        "Native positional reads cannot disturb the managed FD cursor"
    );
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn b4_modal_veto_and_each_write_failure_never_pin_or_replay() {
    use crate::services::claude_tui::submission_fence::{
        self, PreSubmitPersistenceRefused, SubmissionFence,
    };
    struct Veto;
    impl SubmissionFence for Veto {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            Err(PreSubmitPersistenceRefused {
                reason: "veto".into(),
            })
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    for case in ["modal", "veto", "load", "paste", "enter"] {
        let tmux = ScriptedTmux::new(&if case == "modal" {
            format!("Run this tool? allow / deny\n{}", view("draft", false))
        } else {
            view("draft", false)
        });
        let _spy = ready();
        let session = format!("b4-{case}");
        let path = dir.path().join(format!("{case}.jsonl"));
        std::fs::write(&path, "").unwrap();
        binding(&session, &path);
        match case {
            "load" => tmux.fail_send(0),
            "paste" => tmux.fail_send(1),
            "enter" => tmux.fail_send(2),
            _ => {}
        }
        let token = Arc::new(CancelToken::new());
        let fence: Option<Arc<dyn SubmissionFence>> =
            (case == "veto").then(|| Arc::new(Veto) as Arc<dyn SubmissionFence>);
        let result = submission_fence::with_scope(fence, || fresh(&session, &path, "own", &token));
        assert!(result.is_err(), "{case}");
        std::fs::write(&path, format!("{}\n", user("own", "unproven"))).unwrap();
        assert_eq!(
            observed(&token, &path),
            Some(Err(())),
            "{case} cannot activate a writer pin"
        );
        let expected = match case {
            "modal" | "veto" => vec![],
            "load" => vec!["load"],
            "paste" => vec!["load", "paste"],
            _ => vec!["load", "paste", "Enter"],
        };
        assert_eq!(tmux.sends(), expected, "{case} cannot replay");
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}

#[test]
fn b4_guarded_draft_hold_keeps_zero_keys_and_no_native_pin() {
    use crate::services::claude_tui::composer_lock::{self, DraftGuard};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held-draft.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = "b4-guarded-draft";
    binding(session, &path);
    let tmux = ScriptedTmux::new(&view("draft", false));
    composer_lock::guard_draft(session, DraftGuard::RecoveryRequired);
    let _spy = SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(IDLE.into()), 64).collect(),
        draft_capture_at: Some((1, Some("unknown overlay".into()))),
        ..Default::default()
    });
    let token = Arc::new(CancelToken::new());
    assert!(fresh(session, &path, "own", &token).is_err());
    std::fs::write(&path, format!("{}\n", user("own", "unproven"))).unwrap();
    assert_eq!(
        observed(&token, &path),
        None,
        "held preparation never reaches Applied"
    );
    assert!(
        tmux.sends().is_empty(),
        "draft hold cannot submit or replay"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn p2_cap_rejects_partial_nonuser_over_sixteen_mib_through_reader() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cap.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = "p2-cap";
    binding(session, &path);
    let token = Arc::new(CancelToken::new());
    let _tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    fresh(session, &path, "own", &token).unwrap();
    // The observation entry is the real reader, not a direct receipt helper.
    let bytes = format!(
        "{{\"type\":\"system\",\"padding\":\"{}",
        "x".repeat(16 * 1024 * 1024)
    ) + "\n";
    // A short terminal makes the actual reader return after its first observation;
    // the receipt still has to bound the entire non-user range behind it.
    std::fs::write(
        &path,
        format!("{{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}}\n{bytes}"),
    )
    .unwrap();
    assert_eq!(
        observed(&token, &path),
        Some(Err(())),
        "cap includes a partial line"
    );
    assert!(
        !token.native_stop.receipt_sink().retains_receipt(),
        "decided rejection releases its FD"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn p2_new_pane_rearms_a_rejected_token_but_same_pane_cannot() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rearm.jsonl");
    let session = "p2-rearm";
    std::fs::write(&path, "").unwrap();
    binding(session, &path);
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    let token = Arc::new(CancelToken::new());
    tmux.on_enter(&path, &user("foreign", "first-foreign"));
    fresh(session, &path, "own", &token).unwrap();
    assert_eq!(observed(&token, &path), Some(Err(())));
    // Another Applied attempt on the same pane must not erase the first foreign user.
    tmux.on_enter(&path, &user("own", "same-pane-a"));
    fresh(session, &path, "own", &token).unwrap();
    assert_eq!(
        observed(&token, &path),
        Some(Err(())),
        "same incarnation remains immutable"
    );
    std::fs::write(
        crate::services::tmux_common::session_temp_path(session, "spawn_nonce"),
        "new-incarnation",
    )
    .unwrap();
    tmux.on_enter(&path, &user("own", "new-pane-a"));
    fresh(session, &path, "own", &token).unwrap();
    assert!(
        observed(&token, &path).unwrap().is_ok(),
        "new pane must rearm after rejection"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn p2_fixed_jsonl_digest_and_verified_field_round_trip() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("digest.jsonl");
    let session = "p2-digest";
    std::fs::write(&path, "").unwrap();
    binding(session, &path);
    let token = Arc::new(CancelToken::new());
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    let row = r#"{"message":{"content":"own","role":"user"},"sessionId":"receipt-sid","type":"user","uuid":"golden"}"#;
    tmux.on_enter(&path, row);
    fresh(session, &path, "own", &token).unwrap();
    let pin = observed(&token, &path).unwrap().unwrap();
    let serialized = serde_json::to_value(&pin).unwrap();
    let golden = "448daec4cbf17279aa3cffa73e7b4ba652afa2c134ac174f90af48a064241a0c";
    eprintln!("FIXED_JSONL_BLAKE3={golden}");
    assert_eq!(
        serialized["row_digest"], golden,
        "stable, named digest format"
    );
    let decoded: NativeUserPin = serde_json::from_value(serialized).unwrap();
    assert_eq!(decoded, pin);
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn p2_missing_binding_or_unreadable_marker_stays_pending_until_retry() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transient.jsonl");
    let session = "p2-transient";
    std::fs::write(&path, "").unwrap();
    binding(session, &path);
    let token = Arc::new(CancelToken::new());
    let tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    tmux.on_enter(&path, &user("own", "a"));
    fresh(session, &path, "own", &token).unwrap();
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    assert_eq!(
        observed(&token, &path),
        None,
        "temporarily missing binding is pending"
    );
    binding(session, &path);
    let marker = crate::services::tmux_common::session_temp_path(session, "spawn_nonce");
    std::fs::create_dir(&marker).unwrap();
    assert_eq!(
        observed(&token, &path),
        None,
        "unreadable marker is pending"
    );
    std::fs::remove_dir(marker).unwrap();
    assert!(observed(&token, &path).unwrap().is_ok());
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

#[test]
fn p2_reader_scans_each_appended_byte_once_and_releases_decided_fd() {
    use crate::services::claude_tui::input::native_read_byte_total;
    use std::io::Write;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("linear.jsonl");
    let session = "p2-linear";
    std::fs::write(&path, "").unwrap();
    binding(session, &path);
    let token = Arc::new(CancelToken::new());
    let _tmux = ScriptedTmux::new(&view("", false));
    let _spy = ready();
    fresh(session, &path, "own", &token).unwrap();
    let before = native_read_byte_total();
    for _ in 0..12 {
        let row = json!({"type":"system","padding":"x".repeat(64*1024)});
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{row}"
        )
        .unwrap();
        assert_eq!(observed(&token, &path), None);
    }
    let read = native_read_byte_total() - before;
    let growth = std::fs::metadata(&path).unwrap().len();
    assert!(
        read <= growth * 2,
        "reader read {read} for {growth} appended bytes"
    );
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{}",
        user("own", "a")
    )
    .unwrap();
    assert_eq!(observed(&token, &path).unwrap().unwrap().uuid, "a");
    assert!(!token.native_stop.receipt_sink().retains_receipt());
    let decided = native_read_byte_total();
    assert!(observed(&token, &path).unwrap().is_ok());
    assert_eq!(
        native_read_byte_total(),
        decided,
        "cached decision does not reopen or rescan"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}
