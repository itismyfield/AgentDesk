use super::*;
use crate::services::claude_tui::owner_input::{self, Lineage};
use crate::services::tui_input::routine_model::{Requirement, scope_with_publisher};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const NATIVE: &str = "00000000-0000-4000-8000-000000008219";
const PROMPT: &str = "strict Codex input";

#[test]
fn routine_fix2_codex_publisher_failure_precedes_argv_and_owner_submission() {
    scenario(Some("fixture SQL execution failure"), &[]);
    scenario(
        Some("NotSent(\"routine_model_session_record_changed\")"),
        &[],
    );
}

#[test]
fn routine_fix2_codex_owner_submits_once_after_spawn_and_publication() {
    scenario(
        None,
        &["spawn", "publish", "load", "commit", "paste", "prompt"],
    );
}

#[test]
fn routine_fix2_codex_config_without_model_uses_native_default() {
    scenario(None, &["spawn", "argv-prompt"]);
}

fn scenario(failure: Option<&'static str>, expected: &[&str]) {
    use std::io::Write;
    let default_model = expected.contains(&"argv-prompt");
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _guards) = fixture::fixture_after_shared_test_env_lock();
    let _tmux = fixture::fake_tmux(root.path());
    let tmux = root.path().join("tmux");
    std::fs::write(
        &tmux,
        r#"#!/bin/bash
if [ "${1-}" = -u ]; then shift; fi
log="$AGENTDESK_ROOT_DIR/events"
case "${1-}" in
new-session) bash -c "${@: -1}" ;;
list-panes) printf '0\n' ;;
capture-pane) cat "$AGENTDESK_ROOT_DIR/composer" ;;
display-message)
  case "${@: -1}" in
    '#{pid} #{session_id} #{window_id} #{pane_id} #{pane_pid}') printf '123 $1 @1 %%1 234\n' ;;
    *'#{pane_id}|#{pane_pid}|#{pane_width}') printf '%%1|234|80|24|2|22|0\n' ;;
    '#{pane_width} #{pane_height}') printf '80 24\n' ;;
    '#{pane_dead}') printf '0\n' ;;
  esac ;;
load-buffer)
  cat "${@: -1}" > "$AGENTDESK_ROOT_DIR/staged"
  printf 'load\n' >> "$log" ;;
if-shell)
  case "$*" in
    *paste-buffer*) printf 'paste\n' >> "$log" ;;
    *send-keys*Enter*)
      cat "$AGENTDESK_ROOT_DIR/staged" >> "$AGENTDESK_ROOT_DIR/native-input"
      printf 'prompt\n' >> "$log" ;;
    *) exit 1 ;;
  esac
  printf 'CODEX_OWNER_KEY_APPLIED\n' ;;
esac
"#,
    )
    .unwrap();
    std::fs::write(root.path().join("composer"), "\n╭──────────────────────────────────────────────────────────────╮\n│ ▌                                                            │\n╰──────────────────────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send\n").unwrap();
    let binary = root.path().join("codex");
    std::fs::write(
        &binary,
        r#"#!/bin/bash
printf '%s\n' "$@" > "$AGENTDESK_ROOT_DIR/native-argv"
printf 'spawn\n' >> "$AGENTDESK_ROOT_DIR/events"
for arg in "$@"; do
    if [ "$arg" = 'strict Codex input' ]; then
        printf 'argv-prompt\n' >> "$AGENTDESK_ROOT_DIR/events"
    fi
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &binary);
    let home = root.path().join("codex-home");
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "profile = 'work'\n[profiles.work]\nmodel_reasoning_effort = 'medium'\n",
    )
    .unwrap();
    let rollout = sessions.join(format!("rollout-{NATIVE}.jsonl"));
    std::fs::write(&rollout, "{}\n").unwrap();
    let _home = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &home);
    let _hooks = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        "0".as_ref(),
    );
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let desktop = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = desktop.enter();
    let selection = crate::services::codex_tui::session::CodexTuiSessionSelection {
        requested_session_id: Some(NATIVE.into()),
        selected_session_id: Some(NATIVE.into()),
        resume: true,
        reason: "fixture".into(),
        candidate_count: 1,
        rollout_path: Some(rollout),
        rollout_start_offset: Some(3),
    };
    let name = "codex-model-publisher";
    let channel = 8_219_388;
    let working = root.path().to_str().unwrap();
    let overlay = crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
        ProviderKind::Codex,
    );
    let submitted = Arc::new(AtomicUsize::new(0));
    let hook_submitted = submitted.clone();
    let launched = root.path().join("native-argv");
    let events = root.path().join("events");
    let run = || {
        let options = CodexLaunchOptions::new(PROMPT)
            .with_resume_session_id(Some(NATIVE))
            .with_model((!default_model).then_some("routine-model"));
        let launch = super::super::super::prepare_codex_tui_launch_script(
            name,
            Some(NATIVE),
            PROMPT,
            &options,
            (Some(channel), Some(ProviderKind::Codex)),
            false,
            &overlay,
        )
        .unwrap();
        let mut end =
            crate::services::codex_tui::execution_end::producer::LaunchObservation::new(None, name);
        let at_commit = events.clone();
        let _owner = owner_input::install_launch(Box::new(move |lineage, target, _, _, leaf| {
            assert_eq!(lineage, Lineage::Created);
            let commit: owner_input::Commit = if default_model {
                Arc::new(|_| Ok(()))
            } else {
                Arc::new(move |_| {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&at_commit)
                        .unwrap()
                        .write_all(b"commit\n")
                        .unwrap();
                    Ok(())
                })
            };
            owner_input::with_scope(
                "codex".into(),
                target,
                channel,
                commit,
                Arc::new(|_, _| Ok(())),
                Arc::new(|_| Ok(())),
                || leaf(PROMPT.into()),
            )
            .map_err(|error| format!("{error:?}"))?;
            hook_submitted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        create(Create {
            tmux_session_name: name,
            working_dir: working,
            prompt: PROMPT,
            cancel_token: None,
            auth_profile: &overlay.profile_id,
            launch,
            selection: &selection,
            end_observation: &mut end,
        })
    };
    let result = if default_model {
        run()
    } else {
        let argv_at_publish = launched.clone();
        let events_at_publish = events.clone();
        let require = Requirement {
            provider: "codex".into(),
            tmux: name.into(),
            resume: Some(NATIVE.into()),
            model: "routine-model".into(),
        };
        scope_with_publisher(
            require,
            Some(Arc::new(move |proof| {
                let argv = std::fs::read_to_string(&argv_at_publish)
                    .expect("native launch must precede publication");
                let argv = argv.lines().collect::<Vec<_>>();
                assert_eq!(argv.first(), Some(&"resume"));
                assert!(
                    argv.windows(2)
                        .any(|pair| pair == ["-m", proof.model.as_str()])
                );
                assert!(argv.windows(2).any(|pair| pair == [NATIVE, "--"]));
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&events_at_publish)
                    .unwrap()
                    .write_all(b"publish\n")
                    .unwrap();
                failure.map_or(Ok(()), |failure| Err(failure.into()))
            })),
            run,
        )
    };
    let argv = std::fs::read_to_string(launched).unwrap();
    let observed = std::fs::read_to_string(events).unwrap();
    let side_effects = observed.lines().collect::<Vec<_>>();
    if default_model {
        assert!(
            !argv.lines().any(|arg| arg == "-m" || arg == "--model"),
            "{argv}"
        );
        assert_eq!(argv.lines().filter(|arg| *arg == PROMPT).count(), 1);
    } else {
        assert!(
            !argv.lines().any(|arg| arg == PROMPT),
            "native received the prompt before model publication: {argv}"
        );
    }
    if let Some(failure) = failure {
        assert!(
            matches!(&result, Err(reason) if reason.starts_with("NotSent(") && reason.contains(failure)),
            "{:?}",
            result.err()
        );
        assert_eq!(submitted.load(Ordering::SeqCst), 0);
        assert!(!root.path().join("native-input").exists());
        assert_eq!(side_effects, ["spawn", "publish"]);
    } else {
        result.unwrap_or_else(|error| panic!("created Codex input failed: {error}"));
        assert_eq!(submitted.load(Ordering::SeqCst), 1);
        assert_eq!(side_effects, expected);
        if !default_model {
            assert_eq!(
                std::fs::read_to_string(root.path().join("native-input")).unwrap(),
                PROMPT
            );
        }
    }
}
