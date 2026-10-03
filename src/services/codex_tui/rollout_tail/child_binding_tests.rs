use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::codex_tui::session;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::binding::{
    BindingEvents, BindingRecord, BindingTarget, ChannelBindingLog,
};
use crate::services::tui_prompt_dedupe as dedupe;

const SESSION: &str = "codex-child-binding-test";
const CHANNEL: u64 = 655400;
const B: &str = "01a0be20-8543-7b53-b3ac-4f1aabc1b19b";

fn fixture(test: impl FnOnce(&Path, &Path)) {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _home = TestEnvVarGuard::set_path_after_shared_test_env_lock("CODEX_HOME", root.path());
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    struct RestoreLogRoot;
    impl Drop for RestoreLogRoot {
        fn drop(&mut self) {
            dedupe::binding_events::set_test_root(None);
        }
    }
    let _log = RestoreLogRoot;
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    dedupe::register_tmux_channel(SESSION, CHANNEL);
    test(root.path(), &sessions);
    dedupe::reset_state_for_tests();
}

fn child_header(cwd: &Path) -> Value {
    let mut header: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/tui_input/codex-subagent-session-meta.json"
    ))
    .unwrap();
    header["payload"]["cwd"] = serde_json::json!(cwd);
    // Source-only variant ensures the nested indicator is sufficient by itself.
    header["payload"]
        .as_object_mut()
        .unwrap()
        .remove("parent_thread_id");
    header
}

fn write(root: &Path, header: &Value, modified: u64) -> PathBuf {
    let id = header["payload"]["id"].as_str().unwrap();
    let path = root.join(format!("rollout-{id}.jsonl"));
    std::fs::write(&path, format!("{header}\n")).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(modified))
        .unwrap();
    path
}

fn parent(root: &Path, cwd: &Path, id: &str, modified: u64) -> PathBuf {
    write(
        root,
        &serde_json::json!({"type":"session_meta","payload":{
            "id":id,"cwd":cwd,"source":"cli","originator":"codex-tui"
        }}),
        modified,
    )
}

fn binding(path: &Path) -> dedupe::TuiRuntimeBinding {
    dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: super::super::rollout_index::read_rollout_session_meta(path)
            .unwrap()
            .id,
        last_offset: 0,
        relay_last_offset: None,
    }
}

fn events() -> Vec<crate::services::tui_o::writer::binding::BindingEvent> {
    ChannelBindingLog::new(CHANNEL, ShadowProvider::Codex)
        .binding_events_since(CHANNEL, 0)
        .unwrap()
}

#[test]
fn fresh_launch_consumes_b_source_despite_newer_child() {
    fixture(|cwd, root| {
        parent(root, cwd, "01a0be01-eb52-7240-a7e2-318369cf6a6c", 10);
        let b = parent(root, cwd, B, 20);
        write(root, &child_header(cwd), 30);
        let selected = wait_for_latest_rollout_for_cwd(
            cwd,
            SystemTime::UNIX_EPOCH + Duration::from_secs(15),
            root,
            None,
            &mut || false,
            Duration::ZERO,
        )
        .unwrap();
        session::install_launched_codex_tui_runtime_binding(SESSION, Some(0), binding(&selected));
        let events = events();
        assert_eq!(events.len(), 1, "B must publish exactly one source to O");
        let BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        } = &events[0].record
        else {
            panic!("O must consume a resolved source")
        };
        assert_eq!(source.path, b, "O must follow B's own rollout");
        assert_eq!(source.session_id, B);
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION)
                .unwrap()
                .output_path,
            b.display().to_string()
        );
        assert_eq!(
            session::read_codex_tui_rollout_marker(SESSION)
                .unwrap()
                .rollout_path,
            b
        );
    });
}

#[test]
fn child_only_launch_publishes_no_binding_or_marker() {
    fixture(|cwd, root| {
        write(root, &child_header(cwd), 30);
        if let Ok(selected) = wait_for_latest_rollout_for_cwd(
            cwd,
            SystemTime::UNIX_EPOCH,
            root,
            None,
            &mut || false,
            Duration::ZERO,
        ) {
            session::install_launched_codex_tui_runtime_binding(
                SESSION,
                Some(0),
                binding(&selected),
            );
        }
        assert!(
            events().is_empty(),
            "child-only discovery must publish no O source"
        );
        assert!(dedupe::runtime_binding_for_tmux_session(SESSION).is_none());
        assert!(session::read_codex_tui_rollout_marker(SESSION).is_none());
    });
}

#[test]
fn direct_registration_refuses_both_child_indicators() {
    fixture(|cwd, root| {
        let mut header = child_header(cwd);
        let path = write(root, &header, 30);
        let mut proposed = binding(&path);
        proposed.session_id = None;
        dedupe::register_tmux_runtime_binding(SESSION, proposed);
        assert!(
            events().is_empty(),
            "registration must reject source.subagent before O publication"
        );
        header["payload"]["source"] = serde_json::json!("cli");
        header["payload"]["parent_thread_id"] = serde_json::json!(B);
        let path = write(root, &header, 31);
        dedupe::register_launched_tmux_runtime_binding(SESSION, binding(&path));
        assert!(
            events().is_empty(),
            "registration must reject parent_thread_id before O publication"
        );
        let _ = session::write_codex_tui_rollout_marker(SESSION, &path, None);
        assert!(
            session::read_codex_tui_rollout_marker(SESSION).is_none(),
            "direct marker writes must refuse children"
        );
        assert!(dedupe::runtime_binding_for_tmux_session(SESSION).is_none());
    });
}

#[test]
fn replacement_after_discovery_is_rejected_before_marker_and_o_publication() {
    fixture(|cwd, root| {
        let b = parent(root, cwd, B, 20);
        let selected = wait_for_latest_rollout_for_cwd(
            cwd,
            SystemTime::UNIX_EPOCH,
            root,
            None,
            &mut || false,
            Duration::ZERO,
        )
        .unwrap();
        let ready = std::sync::Barrier::new(2);
        let replaced = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                ready.wait();
                std::fs::write(&b, format!("{}\n", child_header(cwd))).unwrap();
                replaced.wait();
            });
            ready.wait();
            replaced.wait();
            session::install_launched_codex_tui_runtime_binding(
                SESSION,
                Some(0),
                binding(&selected),
            );
        });
        assert!(
            events().is_empty(),
            "a child replacing the candidate must never reach O"
        );
        assert!(dedupe::runtime_binding_for_tmux_session(SESSION).is_none());
        assert!(session::read_codex_tui_rollout_marker(SESSION).is_none());
    });
}
