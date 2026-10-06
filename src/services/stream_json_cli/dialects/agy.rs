//! Antigravity CLI dialect (canonical id `antigravity`, binary `agy`).

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::binary_resolver::resolution::finalize_fallback_resolution;
use crate::services::platform::probe_provider_binary_version;
use crate::services::stream_json_cli::codec::AgyCodec;
use crate::services::stream_json_cli::policy::ToolPolicy;
use crate::services::stream_json_cli::request::ProviderTurnRequest;
use crate::services::stream_json_cli::runner::{PreparedCommand, run_prepared};
use crate::services::stream_json_cli::session::parse_strict_uuid;

pub fn execute(request: ProviderTurnRequest, sender: Sender<StreamMessage>) -> Result<(), String> {
    if request.remote_profile.is_some() {
        return Err(
            "NotSupported: Antigravity provider does not support remote execution yet.".to_string(),
        );
    }
    match request.tool_policy.effective_for_stream_json() {
        ToolPolicy::ProviderDefault => {}
        ToolPolicy::ReadOnly | ToolPolicy::AllowListed(_) => {
            return Err(
                "UnsupportedToolPolicy: Antigravity restricted tool policy is not proven".into(),
            );
        }
    }
    let prepared = prepare(&request)?;
    run_prepared(prepared, sender, request.timeout, request.cancel)
}
pub(crate) fn build_argv(request: &ProviderTurnRequest) -> Result<Vec<String>, String> {
    let composed = compose_envelope(
        request.system_prompt.as_deref().unwrap_or(""),
        &request.prompt,
    );
    let mut args = vec![
        "--sandbox".to_string(),
        "--disable-slash-commands".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if let Some(model) = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push("--print".to_string());
    args.push(composed);
    if let Some(session) = &request.session {
        let token = parse_strict_uuid(session.as_str(), "antigravity")?;
        args.push("--conversation".to_string());
        args.push(token.into_inner());
    }

    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--continue" | "-c" | "--new-project" | "--dangerously-skip-permissions" | "--mode"
        )
    }) {
        return Err("AGY dialect refused forbidden flags".into());
    }
    Ok(args)
}
pub(crate) fn prepare(request: &ProviderTurnRequest) -> Result<PreparedCommand, String> {
    let resolution = resolve_agy_binary();
    let executable = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Antigravity CLI (agy) not found".to_string())?;
    let args = build_argv(request)?;

    let redacted_args: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            if index > 0 && args[index - 1] == "--print" {
                "<redacted>".to_string()
            } else {
                arg.clone()
            }
        })
        .collect();

    Ok(PreparedCommand {
        executable: PathBuf::from(executable),
        resolution,
        args,
        redacted_args,
        current_dir: request.working_directory.clone(),
        env: crate::services::provider_auth_profile::overlay_env_pairs(&request.auth_overlay),
        unset_env: crate::services::provider_auth_profile::overlay_unset_keys(
            &request.auth_overlay,
        ),
        codec: Box::new(AgyCodec::new()),
    })
}

fn compose_envelope(system: &str, user: &str) -> String {
    let system_len = system.len();
    format!("SYSTEM_LEN={system_len}\nSYSTEM:\n{system}\nEND_SYSTEM\nUSER:\n{user}\nEND_USER\n")
}

pub fn resolve_agy_binary() -> crate::services::platform::BinaryResolution {
    let resolution = probe_provider_binary_version("agy").resolution;
    if resolution.resolved_path.is_some() {
        return resolution;
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let candidate = PathBuf::from(local).join("agy").join("bin").join("agy.exe");
        if candidate.is_file() {
            return finalize_fallback_resolution(resolution, candidate, "localappdata_agy_bin");
        }
    }
    resolution
}

pub fn resolve_agy_path() -> Option<String> {
    resolve_agy_binary().resolved_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stream_json_cli::policy::ConfiguredToolPolicy;
    use std::time::Duration;

    fn request() -> ProviderTurnRequest {
        ProviderTurnRequest {
            provider: crate::services::provider::ProviderKind::Antigravity,
            prompt: "hello".into(),
            system_prompt: Some("sys".into()),
            tool_policy: ConfiguredToolPolicy::for_new_stream_json_provider(),
            model: None,
            reasoning_effort: None,
            working_directory: PathBuf::from("/tmp"),
            session: None,
            remote_profile: None,
            timeout: Duration::from_secs(120),
            cancel: None,
            auth_overlay: crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                crate::services::provider::ProviderKind::Antigravity,
            ),
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepare_launches_an_agy_found_on_a_relative_path_entry_by_absolute_path() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let bin = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(bin.path().join("agy"), "#!/bin/sh\necho 'agy 1.0.0'\n").expect("stub");
        std::fs::set_permissions(
            bin.path().join("agy"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
        let cwd = std::env::current_dir().expect("cwd");
        let relative: PathBuf = cwd
            .components()
            .skip(1)
            .map(|_| std::path::Path::new(".."))
            .chain(
                bin.path()
                    .components()
                    .skip(1)
                    .map(|part| part.as_os_str().as_ref()),
            )
            .collect();
        let _path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(&relative);
        let _override = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(
            "AGENTDESK_AGY_PATH",
        );
        unsafe { std::env::remove_var("AGENTDESK_AGY_PATH") };

        let prepared = prepare(&request()).expect("agy on a relative PATH entry");
        assert_eq!(prepared.executable, cwd.join(&relative).join("agy"));
        assert!(prepared.executable.is_absolute());
        assert_eq!(prepared.current_dir, PathBuf::from("/tmp"));
    }

    #[cfg(unix)]
    #[test]
    fn localappdata_fallback_runs_the_found_agy_by_absolute_path_from_another_cwd() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::TempDir::new().expect("temp dir");
        let (bin, marker) = (root.path().join("agy/bin"), root.path().join("ran"));
        std::fs::create_dir_all(&bin).expect("bin dir");
        let stub = format!("#!/bin/sh\n: > '{}'\n", marker.display());
        std::fs::write(bin.join("agy.exe"), stub).expect("stub");
        std::fs::set_permissions(bin.join("agy.exe"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let cwd = std::env::current_dir().expect("cwd");
        let relative: PathBuf = cwd
            .components()
            .skip(1)
            .map(|_| std::path::Path::new(".."))
            .chain(
                root.path()
                    .components()
                    .skip(1)
                    .map(|part| part.as_os_str().as_ref()),
            )
            .collect();
        let _local = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "LOCALAPPDATA",
            &relative,
        );
        let _override = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(
            "AGENTDESK_AGY_PATH",
        );
        unsafe { std::env::remove_var("AGENTDESK_AGY_PATH") };
        let mut req = request();
        req.working_directory = root.path().join("a/b/c/d/e/f/g/h/i/j/k/l/m");
        std::fs::create_dir_all(&req.working_directory).expect("child cwd");

        let prepared = prepare(&req).expect("agy from LOCALAPPDATA");
        assert_eq!(
            prepared.executable,
            cwd.join(&relative).join("agy/bin/agy.exe")
        );
        assert!(prepared.executable.is_absolute());
        let _ = run_prepared(
            prepared,
            std::sync::mpsc::channel().0,
            Duration::from_secs(5),
            None,
        );
        assert!(
            marker.exists(),
            "the selected agy must start from the child cwd"
        );
    }

    #[test]
    fn envelope_preserves_lengths() {
        let envelope = compose_envelope("abc", "user\nEND_SYSTEM\n");
        assert!(envelope.contains("SYSTEM_LEN=3"));
        assert!(envelope.contains("USER:\nuser\nEND_SYSTEM\n"));
    }

    #[test]
    fn restricted_policy_fails_before_prepare() {
        let mut req = request();
        req.tool_policy = ConfiguredToolPolicy::Explicit(ToolPolicy::ReadOnly);
        assert!(execute(req, std::sync::mpsc::channel().0).is_err());
    }

    #[test]
    fn default_argv_uses_conversation_not_continue() {
        let args = build_argv(&request()).unwrap();
        assert!(args.contains(&"--sandbox".to_string()));
        assert!(!args.contains(&"--continue".to_string()));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
        assert!(!args.contains(&"--print-timeout".to_string()));
    }
}
