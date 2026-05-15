use std::path::{Path, PathBuf};

use crate::services::claude_tui::hook_bundle::{HookBundleConfig, write_claude_hook_settings};
use crate::services::process::shell_escape;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeTuiSessionFiles {
    pub hook_settings_path: PathBuf,
    pub launch_script_path: PathBuf,
}

impl ClaudeTuiSessionFiles {
    pub fn cleanup_best_effort(&self) {
        let _ = std::fs::remove_file(&self.hook_settings_path);
        let _ = std::fs::remove_file(&self.launch_script_path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeTuiLaunchConfig {
    pub tmux_session_name: String,
    pub working_dir: PathBuf,
    pub claude_bin: PathBuf,
    pub agentdesk_exe: PathBuf,
    pub hook_endpoint: String,
    pub session_id: String,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub resume: bool,
}

impl ClaudeTuiLaunchConfig {
    pub fn session_files(&self) -> ClaudeTuiSessionFiles {
        ClaudeTuiSessionFiles {
            hook_settings_path: PathBuf::from(crate::services::tmux_common::session_temp_path(
                &self.tmux_session_name,
                crate::services::tmux_common::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT,
            )),
            launch_script_path: PathBuf::from(crate::services::tmux_common::session_temp_path(
                &self.tmux_session_name,
                crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
            )),
        }
    }
}

pub fn prepare_claude_tui_launch(
    config: &ClaudeTuiLaunchConfig,
) -> Result<ClaudeTuiSessionFiles, String> {
    let files = config.session_files();
    let result = (|| {
        write_claude_hook_settings(
            &files.hook_settings_path,
            &HookBundleConfig {
                endpoint: config.hook_endpoint.clone(),
                provider: "claude".to_string(),
                session_id: config.session_id.clone(),
                agentdesk_exe: config.agentdesk_exe.display().to_string(),
            },
        )?;
        write_launch_script(&files.launch_script_path, config, &files.hook_settings_path)
    })();
    if let Err(error) = result {
        files.cleanup_best_effort();
        return Err(error);
    }
    Ok(files)
}

pub fn build_claude_tui_args(
    config: &ClaudeTuiLaunchConfig,
    hook_settings_path: &Path,
) -> Vec<String> {
    let mut args = vec!["--dangerously-skip-permissions".to_string()];
    if config.resume {
        args.push("--resume".to_string());
    } else {
        args.push("--session-id".to_string());
    }
    args.push(config.session_id.clone());

    args.push("--settings".to_string());
    args.push(hook_settings_path.display().to_string());

    if let Some(model) = config.model.as_deref().filter(|value| !value.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(system_prompt) = config
        .system_prompt
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        args.push("--append-system-prompt".to_string());
        args.push(system_prompt.to_string());
    }
    args
}

fn write_launch_script(
    path: &Path,
    config: &ClaudeTuiLaunchConfig,
    hook_settings_path: &Path,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("create TUI launch script dir {}: {error}", parent.display())
        })?;
    }
    let args = build_claude_tui_args(config, hook_settings_path)
        .into_iter()
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "#!/bin/bash\n\
         cd {cwd}\n\
         exec {claude_bin} {args}\n",
        cwd = shell_escape(&config.working_dir.display().to_string()),
        claude_bin = shell_escape(&config.claude_bin.display().to_string()),
        args = args,
    );
    std::fs::write(path, script)
        .map_err(|error| format!("write TUI launch script {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> ClaudeTuiLaunchConfig {
        ClaudeTuiLaunchConfig {
            tmux_session_name: "AgentDesk-claude-test".to_string(),
            working_dir: PathBuf::from("/tmp/project dir"),
            claude_bin: PathBuf::from("/usr/local/bin/claude"),
            agentdesk_exe: PathBuf::from("/usr/local/bin/agentdesk"),
            hook_endpoint: "http://127.0.0.1:49152".to_string(),
            session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
            system_prompt: Some("system prompt".to_string()),
            model: Some("sonnet".to_string()),
            resume: false,
        }
    }

    #[test]
    fn tui_args_do_not_use_print_mode() {
        let config = sample_config();
        let args = build_claude_tui_args(&config, Path::new("/tmp/settings.json"));

        assert!(!args.iter().any(|arg| arg == "-p" || arg == "--print"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--session-id", "01234567-89ab-cdef-0123-456789abcdef"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--settings", "/tmp/settings.json"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--model", "sonnet"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--append-system-prompt", "system prompt"])
        );
    }

    #[test]
    fn tui_args_resume_existing_session_by_id() {
        let mut config = sample_config();
        config.resume = true;

        let args = build_claude_tui_args(&config, Path::new("/tmp/settings.json"));

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--resume", "01234567-89ab-cdef-0123-456789abcdef"])
        );
        assert!(!args.iter().any(|arg| arg == "--session-id"));
    }

    #[test]
    fn tui_args_omit_model_when_provider_default_is_requested() {
        let mut config = sample_config();
        config.model = None;

        let args = build_claude_tui_args(&config, Path::new("/tmp/settings.json"));

        assert!(!args.iter().any(|arg| arg == "--model"));
    }

    #[test]
    fn prepare_launch_writes_settings_and_script() {
        let dir = tempfile::tempdir().unwrap();
        let config = sample_config();
        let hook_settings_path = dir.path().join("settings.json");
        let launch_script_path = dir.path().join("launch.sh");

        write_claude_hook_settings(
            &hook_settings_path,
            &HookBundleConfig {
                endpoint: config.hook_endpoint.clone(),
                provider: "claude".to_string(),
                session_id: config.session_id.clone(),
                agentdesk_exe: config.agentdesk_exe.display().to_string(),
            },
        )
        .unwrap();
        write_launch_script(&launch_script_path, &config, &hook_settings_path).unwrap();

        let settings = std::fs::read_to_string(&hook_settings_path).unwrap();
        let script = std::fs::read_to_string(&launch_script_path).unwrap();
        assert!(settings.contains("claude-hook-relay"));
        assert!(script.contains("exec '/usr/local/bin/claude'"));
        assert!(!script.contains(" -p "));
    }

    #[test]
    fn session_files_cleanup_best_effort_removes_settings_and_script() {
        let dir = tempfile::tempdir().unwrap();
        let files = ClaudeTuiSessionFiles {
            hook_settings_path: dir.path().join("settings.json"),
            launch_script_path: dir.path().join("launch.sh"),
        };
        std::fs::write(&files.hook_settings_path, "{}").unwrap();
        std::fs::write(&files.launch_script_path, "#!/bin/bash\n").unwrap();

        files.cleanup_best_effort();
        files.cleanup_best_effort();

        assert!(!files.hook_settings_path.exists());
        assert!(!files.launch_script_path.exists());
    }

    #[test]
    fn prepare_launch_cleans_settings_when_script_write_fails() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
            std::env::set_var("HOSTNAME", "issue-2143-host");
        }

        let mut config = sample_config();
        config.tmux_session_name = format!("issue-2143-{}", uuid::Uuid::new_v4());
        let files = config.session_files();
        std::fs::create_dir_all(files.launch_script_path.parent().unwrap()).unwrap();
        std::fs::create_dir(&files.launch_script_path).unwrap();

        let error = prepare_claude_tui_launch(&config).unwrap_err();

        assert!(error.contains("write TUI launch script"));
        assert!(
            !files.hook_settings_path.exists(),
            "prepare failure must not leave hook settings behind"
        );

        let _ = std::fs::remove_dir_all(&files.launch_script_path);
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }
}
