//! Antigravity CLI dialect (canonical id `antigravity`, binary `agy`).

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::probe_provider_binary_version;
use crate::services::stream_json_cli::codec::AgyCodec;
use crate::services::stream_json_cli::policy::ToolPolicy;
use crate::services::stream_json_cli::request::ProviderTurnRequest;
use crate::services::stream_json_cli::runner::{PreparedCommand, run_prepared};
use crate::services::stream_json_cli::session::parse_strict_uuid;

const PRINT_TIMEOUT_SKEW: Duration = Duration::from_secs(5);
const PRINT_TIMEOUT_FLOOR: Duration = Duration::from_secs(30);
const CLI_DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

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
    if let Some(timeout) = derived_print_timeout(request.timeout) {
        args.push("--print-timeout".to_string());
        args.push(timeout);
    }
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
        codec: Box::new(AgyCodec::new()),
    })
}

fn derived_print_timeout(outer: Duration) -> Option<String> {
    if outer < Duration::from_secs(35) && outer < CLI_DEFAULT_TIMEOUT {
        return None;
    }
    let derived = outer.saturating_sub(PRINT_TIMEOUT_SKEW);
    let secs = derived.max(PRINT_TIMEOUT_FLOOR).as_secs();
    Some(format!("{secs}s"))
}

fn compose_envelope(system: &str, user: &str) -> String {
    let system_len = system.len();
    format!("SYSTEM_LEN={system_len}\nSYSTEM:\n{system}\nEND_SYSTEM\nUSER:\n{user}\nEND_USER\n")
}

pub fn resolve_agy_binary() -> crate::services::platform::BinaryResolution {
    let mut resolution = probe_provider_binary_version("agy").resolution;
    if resolution.resolved_path.is_some() {
        return resolution;
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let candidate = PathBuf::from(local).join("agy").join("bin").join("agy.exe");
        if candidate.is_file() {
            let path = candidate.to_string_lossy().into_owned();
            resolution.resolved_path = Some(path.clone());
            resolution.canonical_path = Some(path.clone());
            resolution.exec_path = Some(path);
            resolution.source = Some("localappdata_agy_bin".into());
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
            provider: crate::services::provider::ProviderKind::Unsupported("antigravity".into()),
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
        }
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
        assert!(args.contains(&"--print-timeout".to_string()));
    }
}
