use super::*;

pub(crate) fn resolve_memento_workspace(
    role_id: &str,
    channel_id: u64,
    workspace_override: Option<&str>,
) -> String {
    resolve_memento_workspace_for_channel(role_id, channel_id, None, workspace_override)
}

pub(crate) fn resolve_memento_workspace_for_channel(
    role_id: &str,
    channel_id: u64,
    channel_name: Option<&str>,
    workspace_override: Option<&str>,
) -> String {
    if let Some(workspace) = workspace_override
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
    {
        return workspace.to_string();
    }

    if channel_id != 0 {
        if let Some(group) = crate::services::discord::settings::resolve_role_binding(
            ChannelId::new(channel_id),
            channel_name,
        )
        .filter(|binding| role_id == UNBOUND_MEMORY_ROLE_ID || binding.role_id == role_id)
        .and_then(|binding| binding.memory.memory_group)
        {
            return group;
        }
        if let Some(workspace) = crate::services::discord::settings::resolve_workspace(
            ChannelId::new(channel_id),
            channel_name,
        )
        .and_then(|workspace| memento_workspace_from_channel_workspace(&workspace))
        {
            return workspace;
        }
    }

    let role_id = role_id.trim();
    if role_id.is_empty() || role_id == UNBOUND_MEMORY_ROLE_ID {
        tracing::warn!(
            channel_id,
            "memento workspace fallback used for unregistered channel"
        );
        return format!("channel-{channel_id}");
    }

    format!("agentdesk-{}", sanitize_memento_workspace_segment(role_id))
}

fn memento_workspace_from_channel_workspace(workspace: &str) -> Option<String> {
    let trimmed = workspace.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if trimmed.contains('/') || trimmed.contains('\\') {
        Path::new(trimmed)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    let sanitized = sanitize_memento_workspace_segment(candidate);
    if sanitized == "default" {
        return None;
    }
    if sanitized == "agentdesk" || sanitized.starts_with("agentdesk-") {
        Some(sanitized)
    } else {
        Some(format!("agentdesk-{sanitized}"))
    }
}

pub(super) fn resolve_remember_agent_id(
    agent_id: Option<String>,
    global: bool,
    workspace: Option<&str>,
    channel_id: Option<u64>,
    channel_name: Option<&str>,
) -> Option<String> {
    if agent_id.is_some() || global || workspace.is_some() {
        return agent_id;
    }
    let channel_id = channel_id.filter(|id| *id != 0)?;
    crate::services::discord::settings::resolve_role_binding(
        ChannelId::new(channel_id),
        channel_name,
    )
    .filter(|binding| binding.memory.memory_group.is_some())
    .map(|binding| resolve_memento_agent_id(&binding.role_id, channel_id))
}

pub(crate) fn resolve_memento_agent_id(role_id: &str, channel_id: u64) -> String {
    let role_id = role_id.trim();
    if role_id.is_empty() || role_id == UNBOUND_MEMORY_ROLE_ID {
        return format!("agentdesk-channel-{channel_id}");
    }
    role_id.to_string()
}

pub(crate) fn sanitize_memento_workspace_segment(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut last_was_dash = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            sanitized.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod workspace_scope_tests {
    use super::*;

    #[test]
    fn memento_workspace_from_channel_workspace_uses_path_basename() {
        assert_eq!(
            memento_workspace_from_channel_workspace("~/.adk/release/workspaces/agentdesk"),
            Some("agentdesk".to_string())
        );
        assert_eq!(
            memento_workspace_from_channel_workspace("~/.adk/release/workspaces/project-agentdesk"),
            Some("agentdesk-project-agentdesk".to_string())
        );
        assert_eq!(
            memento_workspace_from_channel_workspace("Project AgentDesk"),
            Some("agentdesk-project-agentdesk".to_string())
        );
    }

    #[test]
    fn unbound_memento_workspace_falls_back_to_channel_scope() {
        assert_eq!(
            resolve_memento_workspace_for_channel(UNBOUND_MEMORY_ROLE_ID, 4242, None, None),
            "channel-4242"
        );
    }
}
