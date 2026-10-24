use super::*;
use crate::config::TestEnvVarGuard as Guard;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests as fixture};
use std::os::unix::fs::PermissionsExt;

#[test]
fn cold_codex_argv_create_returns_before_rollout_and_failed_create_cleans_its_files() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _guards) = fixture::fixture_after_shared_test_env_lock();
    let _tmux = fixture::fake_tmux(root.path());
    let binary = root.path().join("codex");
    std::fs::write(&binary, "#!/bin/bash\nprintf 'codex-cli 0.160.0\\n'\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &binary);
    let _home = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &root.path().join("home"));
    let _hooks = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        "0".as_ref(),
    );
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    for failed in [false, true] {
        let name = if failed {
            "cold-codex-refused"
        } else {
            "cold-codex-created"
        };
        let prompt = "raw\nvoice _ 'literal'";
        let options = CodexLaunchOptions::new(prompt);
        let overlay = crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
            ProviderKind::Codex,
        );
        let launch = super::super::prepare_codex_tui_launch_script(
            name,
            None,
            prompt,
            &options,
            (None, None),
            false,
            &overlay,
        )
        .unwrap();
        let script_path = launch.script_path.clone();
        let owner_path = launch.owner_path.clone();
        let script = std::fs::read_to_string(&script_path).unwrap();
        assert_eq!(
            script.matches(shell_escape(prompt).as_str()).count(),
            1,
            "cold Codex argv owns the exact prompt bytes once"
        );
        if failed {
            std::fs::write(root.path().join("tmux"), "#!/bin/bash\nexit 1\n").unwrap();
        }
        let selection = crate::services::codex_tui::session::resolve_codex_tui_session(
            None,
            root.path(),
            None,
            true,
        );
        let mut end =
            crate::services::codex_tui::execution_end::producer::LaunchObservation::new(None, name);
        let result = create(Create {
            tmux_session_name: name,
            working_dir: root.path().to_str().unwrap(),
            prompt,
            auth_profile: &overlay.profile_id,
            launch,
            selection: &selection,
            end_observation: &mut end,
        });
        if failed {
            assert!(
                result.is_err(),
                "failed create cannot produce a submit ticket"
            );
            assert!(
                !std::path::Path::new(&script_path).exists()
                    && !std::path::Path::new(&owner_path).exists(),
                "failed Codex create preserves existing cleanup policy"
            );
        } else {
            let submitted =
                result.unwrap_or_else(|e| panic!("cold create must return before rollout: {e}"));
            assert!(
                !root.path().join("home/sessions").exists(),
                "no rollout is needed for physical create"
            );
            assert_eq!(submitted.prepared.context.provider, "codex");
            let calls = std::fs::read_to_string(root.path().join("tmux.calls")).unwrap();
            assert_eq!(
                calls
                    .lines()
                    .filter(|line| line.contains("new-session"))
                    .count(),
                1,
                "cold Codex creates once"
            );
            assert!(
                !calls.contains("paste-buffer") && !calls.contains("send-keys"),
                "argv prompt must never also be pasted"
            );
        }
    }
}
