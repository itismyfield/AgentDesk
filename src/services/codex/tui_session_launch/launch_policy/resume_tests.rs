use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{self, ResumeStart};
use std::os::unix::fs::MetadataExt;

const SESSION: &str = "019e660d-4859-7522-9cee-8ba7c4e7c745";

fn launchable() -> CodexHerdrLaunchable {
    CodexHerdrLaunchable {
        codex_bin: "/usr/bin/true".into(),
        resolution: crate::services::platform::BinaryResolution {
            resolved_path: Some("/usr/bin/true".into()),
            requested_binary: "codex".into(),
            canonical_path: None,
            source: None,
            attempts: vec![],
            failure_kind: None,
            exec_path: None,
        },
        policy: "verified",
    }
}

#[test]
fn codex_herdr_prepare_resumes_only_with_exact_restart_source() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
    let _tmux = binding_context::tests::fake_tmux(root.path());
    let _endpoint_lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let mut prepared = PreparedIncarnation::prepare_at(
        "codex",
        "restart-argv",
        Some(1),
        Some(SESSION),
        true,
        Some(root.path().join("sessions")),
    )
    .unwrap();
    let source = root.path().join("rollout.jsonl");
    std::fs::write(&source, b"{}\n").unwrap();
    let metadata = std::fs::metadata(&source).unwrap();
    prepared.context.resume_start = Some(ResumeStart {
        operation: "restart-operation".into(),
        path: source,
        dev: metadata.dev(),
        ino: metadata.ino(),
        offset: metadata.len(),
    });
    let options = CodexLaunchOptions {
        resume_session_id: Some(SESSION.into()),
        cwd: Some(root.path().display().to_string()),
        model: Some("resume".into()),
        ..Default::default()
    };
    let overlay = crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
        ProviderKind::Codex,
    );
    let command =
        prepare_codex_herdr_launch(&prepared, &launchable(), &options, &overlay, root.path())
            .expect("an exact restart resume must reach the actual Herdr argv preparation");
    assert_eq!(command.cwd, root.path());
    let script = crate::services::tmux_common::session_temp_path("restart-argv", "sh");
    let rendered = std::fs::read_to_string(script).unwrap();
    let exec = rendered
        .lines()
        .find(|line| line.starts_with("exec "))
        .unwrap();
    assert!(
        exec.contains("resume"),
        "the rendered command resumes: {exec}"
    );
    assert!(
        exec.contains(SESSION),
        "the renderer preserves the exact selector: {exec}"
    );
    assert!(
        exec.contains("--no-daemon"),
        "the provider remains isolated: {exec}"
    );
    for invalid in [
        vec!["resume"],
        vec!["resume", "another-session"],
        vec![SESSION],
        vec!["resume", SESSION, "unexpected prompt"],
        vec!["--unknown", "value", "resume", SESSION],
        vec!["resume", SESSION, "resume"],
    ] {
        let args = invalid
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        assert!(
            validate_herdr_launch_args(&prepared, &options, &args).is_err(),
            "a restart must reject a changed rendered command: {invalid:?}"
        );
    }

    for invalid in [
        "missing",
        "operation",
        "selector",
        "inode",
        "offset",
        "zero",
    ] {
        let mut rejected = prepared.context.clone();
        match invalid {
            "missing" => rejected.resume_start = None,
            "operation" => rejected.resume_start.as_mut().unwrap().operation.clear(),
            "selector" => rejected.expected_native_session_id = Some("another-session".into()),
            "inode" => rejected.resume_start.as_mut().unwrap().ino += 1,
            "offset" => rejected.resume_start.as_mut().unwrap().offset += 1,
            "zero" => rejected.resume_start.as_mut().unwrap().offset = 0,
            _ => unreachable!(),
        }
        let rejected = PreparedIncarnation {
            context: rejected,
            path: prepared.path.clone(),
        };
        assert!(
            prepare_codex_herdr_launch(&rejected, &launchable(), &options, &overlay, root.path())
                .is_err(),
            "restart resume must reject {invalid} evidence"
        );
    }
}
