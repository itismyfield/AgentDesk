use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::session_host::herdr_endpoints;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};

const NONCE: &str = "00112233445566778899aabbccddee01";
fn pane(above: &str, input: &str) -> String {
    format!(
        "{above}\n\n────────────────────────────────────────────────────\n❯ {input}\n────────────────────────────────────────────────────\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    )
}

fn bound_target(rig: &HerdrRig, logical: &str) -> (HostedOwner, HerdrTarget) {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: "test".into(),
    };
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: NONCE.into(),
        owner: owner.clone(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(NONCE)),
        source_ref: SourceRef {
            runtime_root: "test".into(),
            channel: "1".into(),
            provider: "claude".into(),
            logical_key: logical.into(),
            execution_nonce: NONCE.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
    };
    (owner, herdr_endpoints().target(&record).unwrap())
}

#[test]
fn herdr_claude_entry_holds_draft_modal_busy_and_failed_capture_without_paste_or_enter() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let context = rig.context(NONCE);
    rig.run_provider(&context, false);
    let logical = "e-claude-composer";
    std::fs::write(
        crate::services::tmux_common::session_temp_path(logical, "host_kind"),
        "herdr",
    )
    .unwrap();
    let (owner, target) = bound_target(&rig, logical);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused_composer_test")
        .unwrap();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let on_send = cancel.clone();
    rig.on_send(move |_| {
        on_send
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let _admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(true, Some(root.path().join("no-stop"))),
    );
    let endpoint = crate::services::herdr_launch::HerdrLaunchEndpoint {
        execution_node: NODE.into(),
        config_key: KEY.into(),
        socket_addr: rig.socket().display().to_string(),
        herdr_session: SESSION.into(),
    };
    for (name, screen) in [
        ("draft", Some(pane("✻ Baked for 2s", "my unsent draft"))),
        (
            "modal",
            Some(pane(
                "Do you want to proceed?\n❯ 1. Yes\nEnter to confirm",
                "",
            )),
        ),
        (
            "busy",
            Some(pane("✳ Architecting… (12s · esc to interrupt)", "")),
        ),
        ("capture failure", None),
    ] {
        if let Some(capture) = screen.as_deref() {
            if name == "draft" {
                assert_eq!(
                    composer_owner(capture),
                    ComposerOwner::Person,
                    "draft_fixture_has_measured_person_ownership_contract"
                );
            }
            if name == "modal" {
                assert!(
                    crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(capture),
                    "modal_fixture_reaches_actual_modal_detector_contract"
                );
            }
        }
        cancel
            .cancelled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let result = screen.map_or_else(|| serde_json::json!({"type":"ok"}), |text| serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE, "source":"recent_unwrapped", "text":text, "revision":1, "truncated":false}}));
        rig.answer("pane.read", result);
        let reads_before = rig
            .requests()
            .iter()
            .filter(|request| request["method"] == "pane.read")
            .count();
        let turn = super::super::HerdrTurn {
            pool: &pool,
            owner: owner.clone(),
            channel_id: 1,
            endpoint: endpoint.clone(),
            row: None,
            prompt: "preserve queued input",
            working_dir: "/tmp",
            session_id: None,
            force_fresh: false,
            system_prompt: None,
            model: None,
            hook_endpoint: None,
            cancel: Some(cancel.clone()),
        };
        let attached = super::super::Attached {
            nonce: NONCE.into(),
            session_id: "same-session".into(),
            transcript: root.path().join("same-transcript.jsonl"),
            target: target.clone(),
            bound: true,
            awaiting_clear: false,
            physical: None,
        };
        let path = attached.transcript.clone();
        crate::services::claude_tui::readiness::test_support::pre_source(logical, &path);
        if name == "busy" {
            std::fs::write(
                &path,
                "{\"type\":\"assistant\",\"message\":{\"content\":\"running\"}}\n",
            )
            .unwrap();
        } else if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        let (sender, _receiver) = std::sync::mpsc::channel();
        assert!(
            super::super::prompt_and_read(
                &turn,
                runtime.handle(),
                attached,
                (
                    crate::services::claude_tui::owner_input::Lineage::Existing,
                    None
                ),
                sender
            )
            .is_err(),
            "composer_guard_refuses_before_input_contract: {name}"
        );
        assert!(
            rig.sends().is_empty(),
            "composer_guard_zero_mutation_contract: {name}"
        );
        if matches!(name, "draft" | "modal") {
            let reads_after = rig
                .requests()
                .iter()
                .filter(|request| request["method"] == "pane.read")
                .count();
            assert_eq!(
                reads_after,
                reads_before + 1,
                "human_composer_refuses_immediately_contract: {name}"
            );
        }
        assert!(
            super::super::not_held(NONCE).is_ok(),
            "an untouched refusal releases only its own transient hold"
        );
    }
    let ready_screen = pane("✻ Baked for 2s", "").replace("❯", "\x1b[36m❯\x1b[0m");
    rig.answer("pane.read", serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE, "source":"recent_unwrapped", "text":ready_screen, "revision":2, "truncated":false}}));
    let plan = [
        TuiInputAction::PasteBuffer("after human draft".into()),
        TuiInputAction::Enter,
    ];
    crate::services::claude_tui::readiness::test_support::pre_source(
        logical,
        &root.path().join("same-transcript.jsonl"),
    );
    assert!(
        matches!(
            run_ready(
                logical,
                &root.path().join("same-transcript.jsonl"),
                &target,
                &plan,
                None
            ),
            InputRun::Applied
        ),
        "human_finished_actual_send_contract"
    );
    assert_eq!(rig.sends().len(), 2);
}

#[test]
fn herdr_claude_composer_waits_from_busy_to_ready_before_exactly_one_submit() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let context = rig.context(NONCE);
    rig.run_provider(&context, false);
    let logical = "i7309-claude-composer";
    std::fs::write(
        crate::services::tmux_common::session_temp_path(logical, "host_kind"),
        "herdr",
    )
    .unwrap();
    let (_owner, target) = bound_target(&rig, logical);
    let _admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(true, Some(root.path().join("no-stop"))),
    );
    let plan = [
        TuiInputAction::PasteBuffer("cold start input".into()),
        TuiInputAction::Enter,
    ];
    let path = root.path().join("live-turn.jsonl");
    crate::services::claude_tui::readiness::test_support::pre_source(logical, &path);
    std::fs::write(
        &path,
        "{\"type\":\"assistant\",\"message\":{\"content\":\"running\"}}\n",
    )
    .unwrap();
    let cancel = CancelToken::new();
    cancel
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        run_ready(logical, &path, &target, &plan, Some(&cancel)),
        InputRun::Cancelled { confirmed: 0 },
        "cancel_before_composer_capture_contract"
    );
    assert!(
        rig.requests().is_empty(),
        "cancelled_composer_reads_zero_contract"
    );
    assert!(
        rig.sends().is_empty(),
        "cancelled_composer_sends_zero_contract"
    );
    cancel
        .cancelled
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let response = |text| {
        serde_json::json!({"type":"pane_read", "read":{
            "pane_id":PANE, "source":"recent_unwrapped", "text":text,
            "revision":1, "truncated":false
        }})
    };
    rig.answer(
        "pane.read",
        response(pane("✳ Architecting… (12s · esc to interrupt)", "")),
    );
    let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = writes.clone();
    rig.on_send(move |_| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let completed = path.clone();
    let finish = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);
        std::fs::write(completed, "{\"type\":\"assistant\",\"message\":{\"content\":\"done\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
    });
    assert_eq!(
        run_ready(logical, &path, &target, &plan, Some(&cancel)),
        InputRun::Applied,
        "busy_then_ready_composer_applies_contract"
    );
    assert_eq!(
        rig.sends().len(),
        2,
        "busy_then_ready_composer_sends_two_contract"
    );
    crate::test_support::wait::join_within(finish, Duration::from_secs(5));
    let reads = rig
        .requests()
        .iter()
        .filter(|request| request["method"] == "pane.read")
        .count();
    // The unproven first look reads once for an open dialog; admission reads once more.
    assert_eq!(
        reads, 2,
        "terminal_signal_admits_while_old_busy_chrome_is_visible"
    );
}

// An `/exit` sends nothing while a draft is stashed under an empty composer, where a prompt goes on;
// with no stash both are sent.
#[test]
fn an_exit_is_never_sent_over_a_stashed_draft() {
    let clear = pane("✻ Baked for 2s", "");
    let stash_row = format!("{:>50}", "\u{203a} stashed");
    let stashed = clear.replacen("\n\n", &format!("\n{stash_row}\n"), 1);
    for (i, (capture, exit, sent)) in [
        (&stashed, true, false),
        (&stashed, false, true),
        (&clear, true, true),
    ]
    .into_iter()
    .enumerate()
    {
        let called = std::cell::Cell::new(false);
        let send = || {
            called.set(true);
            InputRun::Applied
        };
        let capture = || Some(capture.clone());
        let run = match exit {
            true => when_ready_to_exit(capture, None, send),
            false => {
                let source = tempfile::tempdir().unwrap();
                let logical = format!("stash-composer-{i}");
                let path = source.path().join("turn.jsonl");
                crate::services::claude_tui::readiness::test_support::pre_source(&logical, &path);
                when_idle(&logical, &path, capture, None, send)
            }
        };
        assert_eq!(called.get(), sent, "exit {exit}: {run:?}");
    }
}

#[test]
fn herdr_ready_revalidates_a_source_changed_by_the_capture_before_any_key() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let context = rig.context(NONCE);
    rig.run_provider(&context, false);
    let logical = "composer-source-flip";
    std::fs::write(
        crate::services::tmux_common::session_temp_path(logical, "host_kind"),
        "herdr",
    )
    .unwrap();
    let (_, target) = bound_target(&rig, logical);
    let _admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(true, Some(root.path().join("no-stop"))),
    );
    let path = root.path().join("original.jsonl");
    crate::services::claude_tui::readiness::test_support::pre_source(logical, &path);
    let original = path.clone();
    std::fs::write(
        &path,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    let replacement = root.path().join("replacement.jsonl");
    std::fs::write(
        &replacement,
        "{\"type\":\"assistant\",\"message\":{\"content\":\"new source is Running\"}}\n",
    )
    .unwrap();
    rig.answer("pane.read", serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE,"source":"recent_unwrapped","text":pane("✻ Baked for 2s", ""),"revision":1,"truncated":false}}));
    let switched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = switched.clone();
    rig.on_reply(move |_| {
        if seen.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        std::fs::rename(&replacement, &original).unwrap();
    });
    let plan = [
        TuiInputAction::PasteBuffer("one original".into()),
        TuiInputAction::Enter,
    ];
    let result = run_ready(logical, &path, &target, &plan, None);
    eprintln!(
        "changed_source_got_keys={} outcome={result:?}",
        rig.sends().len()
    );
    assert!(
        switched.load(std::sync::atomic::Ordering::SeqCst),
        "must reach the real capture after current-source observation"
    );
    assert!(
        rig.sends().is_empty(),
        "source changed after observation must get zero keys"
    );
    assert!(matches!(result, InputRun::Refused(_)));
}

// A Herdr execution relaunched with `--resume` idles at its composer over whatever its old
// transcript ends with; its own SessionStart admits the first queued input before any new turn.
#[test]
fn a_resumed_execution_takes_its_first_input_before_any_new_turn_ends() {
    use crate::services::claude_tui::{hook_server::HookEventKind, readiness};
    use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding, binding_context};
    use serde_json::json;
    let old = chrono::Utc::now() - chrono::Duration::hours(1);
    let said = |text: &str| json!({"type":"assistant","timestamp":old,"message":{"role":"assistant","content":[{"type":"text","text":text}]}});
    let exit = [json!({"type":"last-prompt"}), json!({"type":"cost-state"})];
    let finished = [
        said("earlier answer"),
        json!({"type":"system","subtype":"stop_hook_summary","timestamp":old}),
        json!({"type":"system","subtype":"turn_duration","timestamp":old}),
    ];
    let cut_short = [
        json!({"type":"user","timestamp":old,"message":{"role":"user","content":"earlier prompt"}}),
        json!({"type":"assistant","timestamp":old,"message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Read","input":{}}]}}),
        json!({"type":"permission-mode","permissionMode":"bypassPermissions"}),
    ];
    for (name, history) in [("finished", &finished[..]), ("cut short", &cut_short[..])] {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        readiness::initialize();
        let rig = HerdrRig::start();
        let _registry = rig.registry_on_this_thread();
        let logical = format!("i8161-resumed-{}", uuid::Uuid::new_v4().simple());
        let native = uuid::Uuid::new_v4().to_string();
        let projects = root.path().join("projects");
        let path = projects.join("workspace").join(format!("{native}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows = |rows: &[serde_json::Value]| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            for row in rows {
                let mut row = row.clone();
                row["sessionId"] = json!(native);
                writeln!(file, "{row}").unwrap();
            }
        };
        rows(history);
        rows(&exit);
        let context: binding_context::BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":chrono::Utc::now(),"execution_nonce":NONCE,
            "tmux_session":logical,"channel_id":1,"owner_runtime_root":"test","host":null,
            "expected_native_session_id":native,"launch_mode":"resume","provider_root":projects
        }))
        .unwrap();
        let launch = binding_context::PreparedIncarnation::create(context.clone()).unwrap();
        rig.run_provider(&launch.path, false);
        for (marker, value) in [("host_kind", "herdr"), ("spawn_nonce", NONCE)] {
            let marker = crate::services::tmux_common::session_temp_path(&logical, marker);
            std::fs::write(marker, value).unwrap();
        }
        let (_owner, target) = bound_target(&rig, &logical);
        let _admission = crate::services::herdr_admission::force_for_test(
            crate::services::herdr_admission::Admission::new(
                true,
                Some(root.path().join("no-stop")),
            ),
        );
        readiness::note_hook(
            "claude",
            &HookEventKind::SessionStart,
            &logical,
            &json!({"session_id":native,"transcript_path":path,"source":"resume"}),
            Some(&context),
            Some(chrono::Utc::now()),
            false,
        );
        // The launch's attach names the resumed transcript; the resume then adds its own metadata.
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &logical,
            TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(native.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        rows(&[json!({"type":"tools-used"})]);
        rig.answer("pane.read", json!({"type":"pane_read", "read":{"pane_id":PANE,"source":"recent_unwrapped","text":pane("✻ Baked for 2s", ""),"revision":1,"truncated":false}}));
        let plan = [
            TuiInputAction::PasteBuffer("queued while it resumed".into()),
            TuiInputAction::Enter,
        ];
        assert_eq!(
            run_ready(&logical, &path, &target, &plan, None),
            InputRun::Applied,
            "{name}: a resumed idle execution takes the queued input"
        );
        assert_eq!(rig.sends().len(), 2, "{name}: one paste and one Enter");
        tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
    }
}

/// Q5, isolated Claude Code v2.1.325 pane: a UserPromptSubmit hook still running.
const HOOK_RUNNING: &str = "❯ reply with the single word pong\n✽ Marinating… (running UserPromptSubmit hooks… 1/2 · 2s)\n                                                    ● high · /effort";
/// Q5: the turn right after its prompt, before any transcript row can be read back.
const TURN_STARTING: &str = "❯ reply with the single word pong\n✽ Fiddle-faddling…";
/// Q6, 게임-cc 2026-11-15 17:22: the turn ended, one background shell still runs.
const BACKGROUND_SHELL: &str =
    "  그때 따로 처리할게요.\n\n✻ Worked for 2m 10s · done 5:19 PM · 1 shell\n  still running";

// The daemon restarted under a pane launched before it; nothing after the last hard
// terminal, no Start of this run. S1 gives it exactly one input on an idle, empty screen only.
#[test]
fn a_pane_that_outlived_the_restart_takes_one_input_only_on_an_idle_screen() {
    use crate::services::claude_tui::readiness;
    use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding, binding_context};
    use serde_json::json;
    let old = chrono::Utc::now() - chrono::Duration::hours(6);
    let finished = [
        json!({"type":"user","timestamp":old,"message":{"role":"user","content":"earlier prompt"}}),
        json!({"type":"assistant","timestamp":old,"message":{"role":"assistant","content":[{"type":"text","text":"earlier answer"}]}}),
        json!({"type":"system","subtype":"stop_hook_summary","timestamp":old}),
        json!({"type":"system","subtype":"turn_duration","timestamp":old}),
    ];
    let mcp = "✻ Baked for 2s\n\n ⚠ 1 MCP server needs authentication · run /mcp";
    use crate::services::claude_tui::host_input::InputRefusal::{Unknown, Unproven, Waits};
    // `None` is admitted; a refusal names what the budget's end saw; a running screen over
    // unchanged idle bytes is the session's own turn and waits for its end.
    let cases = [
        ("idle", true, pane("✻ Baked for 2s", ""), None),
        ("stale MCP auth banner", true, pane(mcp, ""), None),
        ("background shell", true, pane(BACKGROUND_SHELL, ""), None),
        (
            "MCP banner + draft",
            true,
            pane(mcp, "a person's draft"),
            Some(Unknown),
        ),
        (
            "MCP banner + modal",
            true,
            pane(
                &format!("{mcp}\nDo you want to proceed?\n❯ 1. Yes\nEnter to confirm"),
                "",
            ),
            Some(Unknown),
        ),
        (
            "MCP banner + hook running",
            true,
            pane(&format!("{mcp}\n{HOOK_RUNNING}"), ""),
            Some(Waits(readiness::WaitCause::OwnTurn)),
        ),
        (
            "MCP banner + turn starting",
            true,
            pane(&format!("{mcp}\n{TURN_STARTING}"), ""),
            Some(Waits(readiness::WaitCause::OwnTurn)),
        ),
        (
            "new incarnation",
            false,
            pane("✻ Baked for 2s", ""),
            Some(Unproven(readiness::Unproven::Retained)),
        ),
    ];
    for (name, older, screen, refused) in cases {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        readiness::initialize();
        // Stamped after the clock starts, as the daemon boots before any launch.
        let created_at = if older { old } else { chrono::Utc::now() };
        let rig = HerdrRig::start();
        let _registry = rig.registry_on_this_thread();
        let logical = format!("i8161-restored-{}", uuid::Uuid::new_v4().simple());
        let native = uuid::Uuid::new_v4().to_string();
        let projects = root.path().join("projects");
        let path = projects.join("workspace").join(format!("{native}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&path).unwrap();
            for row in &finished {
                let mut row = row.clone();
                row["sessionId"] = json!(native);
                writeln!(file, "{row}").unwrap();
            }
        }
        let context: binding_context::BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":created_at,"execution_nonce":NONCE,
            "tmux_session":logical,"channel_id":1,"owner_runtime_root":"test","host":null,
            "expected_native_session_id":native,"launch_mode":"resume","provider_root":projects
        }))
        .unwrap();
        let launch = binding_context::PreparedIncarnation::create(context).unwrap();
        rig.run_provider(&launch.path, false);
        for (marker, value) in [("host_kind", "herdr"), ("spawn_nonce", NONCE)] {
            let marker = crate::services::tmux_common::session_temp_path(&logical, marker);
            std::fs::write(marker, value).unwrap();
        }
        let (_owner, target) = bound_target(&rig, &logical);
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &logical,
            TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(native.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        readiness::test_support::daemon_restart(&logical);
        assert!(
            readiness::observe(&logical, Some(&path)).is_none(),
            "{name}: the shared observe keeps the restart floor"
        );
        rig.answer("pane.read", json!({"type":"pane_read", "read":{"pane_id":PANE,"source":"recent_unwrapped","text":screen,"revision":1,"truncated":false}}));
        let plan = [
            TuiInputAction::PasteBuffer("one original".into()),
            TuiInputAction::Enter,
        ];
        READY_WAIT_FOR_TESTS.with(|wait| wait.set(Some(Duration::from_millis(600))));
        let result = run_ready(&logical, &path, &target, &plan, None);
        READY_WAIT_FOR_TESTS.with(|wait| wait.set(None));
        match refused {
            None => {
                assert_eq!(result, InputRun::Applied, "{name}");
                assert_eq!(rig.sends().len(), 2, "{name}: one paste and one Enter");
            }
            Some(kind) => {
                assert_eq!(result, InputRun::Refused(kind), "{name}");
                assert!(rig.sends().is_empty(), "{name}: no key");
            }
        }
        tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
    }
}

#[test]
fn restored_screen_reads_marker_draft_modal_and_busy_but_not_a_stale_auth_banner() {
    let mcp = "✻ Baked for 2s\n\n ⚠ 1 MCP server needs authentication · run /mcp";
    assert!(restored_screen_idle(&pane(mcp, "")));
    assert!(restored_screen_idle(&pane(BACKGROUND_SHELL, "")));
    for (name, screen) in [
        ("draft", pane(mcp, "draft")),
        ("hook running", pane(HOOK_RUNNING, "")),
        ("turn starting", pane(TURN_STARTING, "")),
        (
            "interrupt chrome",
            pane("✳ Architecting… (12s · esc to interrupt)", ""),
        ),
        (
            "modal",
            pane("Do you want to proceed?\n❯ 1. Yes\nEnter to confirm", ""),
        ),
        ("no composer", "✻ Baked for 2s".to_string()),
    ] {
        assert!(!restored_screen_idle(&screen), "{name}");
    }
}

/// A wait that ends names its open cause; an older launch's lost source settles. The dialog, a
/// background agent's permission prompt over the composer, holds at once as a dialog.
#[test]
fn a_held_first_input_names_its_wait_and_a_lost_older_source_settles() {
    use crate::services::claude_tui::host_input::InputRefusal::{Unproven, Waits};
    use crate::services::claude_tui::readiness::{self, WaitCause};
    use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding, binding_context};
    use serde_json::json;
    let dialog = "─────────────────────────────────────────────\n Bash command · from the adk-lane agent\n Recapture r3 shots on final build\n │ cd /tmp/dashboard && node capture.mjs\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel · Tab to amend · ctrl+x ctrl+k twice to stop background agen…";
    let old = chrono::Utc::now() - chrono::Duration::hours(6);
    // (name, launched before this run, binding kept, screen, expected refusal)
    let cases = [
        (
            "dialog over a proven source",
            true,
            true,
            dialog.to_string(),
            Some(Waits(WaitCause::Modal)),
        ),
        (
            "an answer naming Esc to cancel over an empty composer",
            true,
            true,
            pane("Press \u{1b}[1mEsc\u{1b}[0m to cancel the operation.", ""),
            None,
        ),
        (
            "older launch lost its source",
            true,
            false,
            pane("✻ Baked for 2s", ""),
            Some(Unproven(readiness::Unproven::Lost)),
        ),
        (
            "this run's launch has no source yet",
            false,
            false,
            pane("✻ Baked for 2s", ""),
            Some(Waits(WaitCause::NoSource)),
        ),
    ];
    for (name, older, bound, screen, refused) in cases {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        readiness::initialize();
        let created_at = if older { old } else { chrono::Utc::now() };
        let rig = HerdrRig::start();
        let _registry = rig.registry_on_this_thread();
        let logical = format!("i8161-s0-{}", uuid::Uuid::new_v4().simple());
        let native = uuid::Uuid::new_v4().to_string();
        let projects = root.path().join("projects");
        let path = projects.join("workspace").join(format!("{native}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let write = |rows: &[serde_json::Value]| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            for row in rows {
                let mut row = row.clone();
                row["sessionId"] = json!(native);
                writeln!(file, "{row}").unwrap();
            }
        };
        let turn = |at: chrono::DateTime<chrono::Utc>| {
            [
                json!({"type":"user","timestamp":at,"message":{"role":"user","content":"<task-notification>"}}),
                json!({"type":"assistant","timestamp":at,"message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}),
                json!({"type":"system","subtype":"stop_hook_summary","timestamp":at}),
                json!({"type":"system","subtype":"turn_duration","timestamp":at}),
            ]
        };
        write(&turn(old));
        let context: binding_context::BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":created_at,"execution_nonce":NONCE,
            "tmux_session":logical,"channel_id":1,"owner_runtime_root":"test","host":null,
            "expected_native_session_id":native,"launch_mode":"resume","provider_root":projects
        }))
        .unwrap();
        let launch = binding_context::PreparedIncarnation::create(context).unwrap();
        rig.run_provider(&launch.path, false);
        for (marker, value) in [("host_kind", "herdr"), ("spawn_nonce", NONCE)] {
            let marker = crate::services::tmux_common::session_temp_path(&logical, marker);
            std::fs::write(marker, value).unwrap();
        }
        let (_owner, target) = bound_target(&rig, &logical);
        readiness::test_support::daemon_restart(&logical);
        if bound {
            tui_prompt_dedupe::register_tmux_runtime_binding(
                &logical,
                TuiRuntimeBinding {
                    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                    output_path: path.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some(native.clone()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            // The restart floor is set; an internal turn after it proves the source again.
            assert!(
                readiness::observe(&logical, Some(&path)).is_none(),
                "{name}: floor"
            );
            write(&turn(chrono::Utc::now()));
            assert!(
                readiness::observe(&logical, Some(&path)).is_some(),
                "{name}: proven"
            );
        }
        rig.answer("pane.read", json!({"type":"pane_read", "read":{"pane_id":PANE,"source":"recent_unwrapped","text":screen,"revision":1,"truncated":false}}));
        let plan = [
            TuiInputAction::PasteBuffer("one original".into()),
            TuiInputAction::Enter,
        ];
        READY_WAIT_FOR_TESTS.with(|wait| wait.set(Some(Duration::from_millis(600))));
        let result = run_ready(&logical, &path, &target, &plan, None);
        READY_WAIT_FOR_TESTS.with(|wait| wait.set(None));
        let Some(refused) = refused else {
            assert_eq!(result, InputRun::Applied, "{name}");
            assert_eq!(rig.sends().len(), 2, "{name}: one paste and one Enter");
            tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
            continue;
        };
        assert_eq!(result, InputRun::Refused(refused), "{name}");
        assert!(rig.sends().is_empty(), "{name}: no key");
        // The hold detail names the cause the operator log never could.
        if let InputRun::Refused(Waits(cause)) = result {
            assert!(
                format!("{result:?}").contains(&format!("{cause:?}")),
                "{name}"
            );
        }
        tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
    }
}
