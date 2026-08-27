//! Grok Build TUI dialect (`grok -p --output-format streaming-messages-json`).

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::probe_provider_binary_version;
use crate::services::stream_json_cli::codec::MessagesJsonCodec;
use crate::services::stream_json_cli::policy::{AgentTool, ToolPolicy};
use crate::services::stream_json_cli::request::ProviderTurnRequest;
use crate::services::stream_json_cli::runner::{PreparedCommand, run_prepared};
use crate::services::stream_json_cli::session::parse_strict_uuid;

const GROK_READ_TOOLS: &[(&str, &str)] = &[
    ("read", "read_file"),
    ("grep", "grep"),
    ("glob", "list_dir"),
];

pub fn execute(request: ProviderTurnRequest, sender: Sender<StreamMessage>) -> Result<(), String> {
    if request.remote_profile.is_some() {
        return Err(
            "NotSupported: Grok provider does not support remote execution yet.".to_string(),
        );
    }
    let prepared = prepare(&request)?;
    run_prepared(prepared, sender, request.cancel)
}

pub(crate) fn build_argv(request: &ProviderTurnRequest) -> Result<Vec<String>, String> {
    let policy = request.tool_policy.effective_for_stream_json();
    let mut args = vec![
        "-p".to_string(),
        request.prompt.clone(),
        "--output-format".to_string(),
        "streaming-messages-json".to_string(),
        "--cwd".to_string(),
        request.working_directory.to_string_lossy().into_owned(),
        "--verbatim".to_string(),
        "--no-auto-update".to_string(),
        "--no-memory".to_string(),
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
    if let Some(rules) = request
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("--rules".to_string());
        args.push(rules.to_string());
    }
    match &policy {
        ToolPolicy::ProviderDefault => {}
        ToolPolicy::ReadOnly => append_readonly_flags(&mut args, &readonly_tool_set())?,
        ToolPolicy::AllowListed(tools) => append_readonly_flags(&mut args, tools)?,
    }
    if let Some(session) = &request.session {
        let token = parse_strict_uuid(session.as_str(), "grok")?;
        args.push("--resume".to_string());
        args.push(token.into_inner());
    }

    if args
        .iter()
        .any(|arg| arg == "--continue" || arg == "-s" || arg == "--session-id")
    {
        return Err("Grok dialect refused forbidden session flags".into());
    }
    Ok(args)
}

pub(crate) fn prepare(request: &ProviderTurnRequest) -> Result<PreparedCommand, String> {
    let resolution = resolve_grok_binary();
    let executable = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Grok CLI not found".to_string())?;
    let args = build_argv(request)?;

    let redacted_args: Vec<String> = args
        .iter()
        .map(|arg| {
            if arg == &request.prompt
                || request
                    .system_prompt
                    .as_deref()
                    .is_some_and(|rules| rules == arg)
            {
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
        codec: Box::new(MessagesJsonCodec::new()),
    })
}

fn readonly_tool_set() -> std::collections::BTreeSet<AgentTool> {
    ["Read", "Grep", "Glob"]
        .into_iter()
        .map(AgentTool::new)
        .collect()
}

fn append_readonly_flags(
    args: &mut Vec<String>,
    tools: &std::collections::BTreeSet<AgentTool>,
) -> Result<(), String> {
    if tools.is_empty() {
        return Err("UnsupportedToolPolicy: empty Grok allowlist".into());
    }
    let mut mapped = Vec::new();
    for tool in tools {
        let Some((_, native)) = GROK_READ_TOOLS
            .iter()
            .find(|(name, _)| *name == tool.canonical_key())
        else {
            return Err(format!(
                "UnsupportedToolPolicy: Grok has no native mapping for {}",
                tool.as_str()
            ));
        };
        mapped.push(*native);
    }
    mapped.sort_unstable();
    mapped.dedup();
    args.push("--tools".to_string());
    args.push(mapped.join(","));
    args.push("--disallowed-tools".to_string());
    args.push("Agent".to_string());
    args.push("--no-subagents".to_string());
    args.push("--disable-web-search".to_string());
    args.push("--deny".to_string());
    args.push("MCPTool(*)".to_string());
    Ok(())
}

pub fn resolve_grok_binary() -> crate::services::platform::BinaryResolution {
    let mut resolution = probe_provider_binary_version("grok").resolution;
    if resolution.resolved_path.is_some() {
        return resolution;
    }
    if let Some(home) = dirs::home_dir() {
        for candidate in [
            home.join(".grok").join("bin").join("grok"),
            home.join(".grok").join("bin").join("grok.exe"),
        ] {
            if candidate.is_file() {
                let path = candidate.to_string_lossy().into_owned();
                resolution.resolved_path = Some(path.clone());
                resolution.canonical_path = Some(path.clone());
                resolution.exec_path = Some(path);
                resolution.source = Some("grok_home_bin".into());
                break;
            }
        }
    }
    resolution
}

pub fn resolve_grok_path() -> Option<String> {
    resolve_grok_binary().resolved_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stream_json_cli::policy::ConfiguredToolPolicy;
    use crate::services::stream_json_cli::session::parse_strict_uuid;
    use std::time::Duration;

    fn request(policy: ConfiguredToolPolicy) -> ProviderTurnRequest {
        ProviderTurnRequest {
            provider: crate::services::provider::ProviderKind::Unsupported("grok".into()),
            prompt: "hello".into(),
            system_prompt: Some("be brief".into()),
            tool_policy: policy,
            model: None,
            working_directory: PathBuf::from("/tmp"),
            session: None,
            remote_profile: None,
            timeout: Duration::from_secs(60),
            cancel: None,
        }
    }

    #[test]
    fn provider_default_argv_has_no_resume_or_session_id() {
        let args = build_argv(&request(
            ConfiguredToolPolicy::for_new_stream_json_provider(),
        ))
        .expect("argv");
        assert!(!args.contains(&"--yolo".to_string()));
        assert!(args.contains(&"--rules".to_string()));
        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.iter().any(|arg| arg == "-s" || arg == "--continue"));
        assert!(!args.contains(&"--system-prompt-override".to_string()));
    }

    #[test]
    fn readonly_mapping_is_fail_closed_for_write_tools() {
        let mut tools = std::collections::BTreeSet::new();
        tools.insert(AgentTool::new("Write"));
        let req = request(ConfiguredToolPolicy::Explicit(ToolPolicy::AllowListed(
            tools,
        )));
        assert!(build_argv(&req).is_err());
    }

    #[test]
    fn resume_requires_uuid() {
        let mut req = request(ConfiguredToolPolicy::for_new_stream_json_provider());
        req.session =
            Some(parse_strict_uuid("01234567-89ab-cdef-0123-456789abcdef", "grok").unwrap());
        let args = build_argv(&req).unwrap();
        assert!(args.windows(2).any(|pair| pair[0] == "--resume"));
    }
}
