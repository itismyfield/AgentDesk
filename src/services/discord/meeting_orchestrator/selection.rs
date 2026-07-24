use super::*;

fn compact_selection_reason(reason: &str) -> Option<String> {
    let compact = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = compact.trim();

    for prefix in [
        "선정 사유:",
        "selection_reason:",
        "selection reason:",
        "reason:",
        "rationale:",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            trimmed = rest.trim();
            break;
        }
    }

    trimmed = trimmed.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | '“' | '”'));
    trimmed = trimmed
        .trim_start_matches(|ch: char| matches!(ch, '-' | '*' | '•' | '1'..='9' | '.' | ')'));
    trimmed = trimmed.trim();

    if trimmed.is_empty() {
        return None;
    }

    Some(trimmed.to_string())
}

fn normalize_selection_reason(reason: &str) -> Option<String> {
    compact_selection_reason(reason)
}

fn build_meeting_start_status_message(
    agenda: &str,
    meeting_hash_display: &str,
    thread_hash_display: Option<&str>,
    primary_provider: &ProviderKind,
    reviewer_provider: &ProviderKind,
    selection_reason: Option<&str>,
) -> String {
    let thread_hash_line = thread_hash_display
        .map(|hash| format!("\n스레드 해시: {hash}"))
        .unwrap_or_default();
    let selection_reason_line = selection_reason
        .and_then(normalize_selection_reason)
        .map(|reason| format!("\n선정 사유: {reason}"))
        .unwrap_or_default();

    format!(
        "📋 **라운드 테이블 회의 시작**\n안건: {}\n회의 해시: {}{}\n진행 프로바이더: {} / 리뷰 프로바이더: {}\n참여자 선정 중...{}",
        agenda,
        meeting_hash_display,
        thread_hash_line,
        primary_provider.display_name(),
        reviewer_provider.display_name(),
        selection_reason_line
    )
}

fn clamp_max_participants(max_participants: usize) -> usize {
    max_participants.clamp(MIN_MEETING_PARTICIPANTS, DEFAULT_MAX_PARTICIPANTS)
}

fn csv_or_missing(values: &[String]) -> String {
    if values.is_empty() {
        "metadata_missing".to_string()
    } else {
        values.join(", ")
    }
}

fn agent_metadata_card(agent: &MeetingAgentConfig) -> String {
    let mut missing = Vec::new();
    if agent.keywords.is_empty() {
        missing.push("keywords");
    }
    if agent
        .domain_summary
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        missing.push("domain_summary");
    }
    if agent.strengths.is_empty() {
        missing.push("strengths");
    }
    if agent.task_types.is_empty() {
        missing.push("task_types");
    }
    if agent.anti_signals.is_empty() {
        missing.push("anti_signals");
    }
    if agent
        .provider_hint
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        missing.push("provider_hint");
    }

    let provider = agent
        .provider
        .as_ref()
        .map(ProviderKind::display_name)
        .or_else(|| {
            agent.provider_hint.as_deref().map(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    "metadata_missing"
                } else {
                    trimmed
                }
            })
        })
        .unwrap_or("metadata_missing");
    let model = agent
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("metadata_missing");
    let reasoning_effort = agent
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("metadata_missing");
    let selection_profile = format!(
        "{} | provider={} | strengths={} | task_types={}",
        agent.display_name,
        provider,
        csv_or_missing(&agent.strengths),
        csv_or_missing(&agent.task_types),
    );

    format!(
        r#"- role_id: {role_id}
  display_name: {display_name}
  selection_profile: {selection_profile}
  keywords: {keywords}
  domain_summary: {domain_summary}
  strengths: {strengths}
  task_types: {task_types}
  anti_signals: {anti_signals}
  provider: {provider}
  provider_hint: {provider_hint}
  model: {model}
  reasoning_effort: {reasoning_effort}
  metadata_missing: {metadata_missing}"#,
        role_id = agent.role_id,
        display_name = agent.display_name,
        selection_profile = selection_profile,
        keywords = csv_or_missing(&agent.keywords),
        domain_summary = agent
            .domain_summary
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("metadata_missing"),
        strengths = csv_or_missing(&agent.strengths),
        task_types = csv_or_missing(&agent.task_types),
        anti_signals = csv_or_missing(&agent.anti_signals),
        provider = provider,
        provider_hint = agent
            .provider_hint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("metadata_missing"),
        model = model,
        reasoning_effort = reasoning_effort,
        metadata_missing = if missing.is_empty() {
            "[]".to_string()
        } else {
            format!("[{}]", missing.join(", "))
        },
    )
}

fn summary_agent_context(config: &MeetingConfig, resolved_summary_agent: &str) -> String {
    let Some(agent) = config
        .available_agents
        .iter()
        .find(|a| a.role_id == resolved_summary_agent)
    else {
        return format!(
            "summary_agent `{}` is not in the meeting candidate pool. Keep this summary persona as a fallback and do not replace it with a participant persona.",
            resolved_summary_agent
        );
    };

    if agent.prompt_file.trim().is_empty() {
        return format!(
            "summary_agent `{}` has no prompt file. Keep the `{}` summary persona and produce a neutral meeting record.",
            resolved_summary_agent, agent.display_name
        );
    }

    load_role_prompt(&RoleBinding {
        role_id: resolved_summary_agent.to_string(),
        prompt_file: agent.prompt_file.clone(),
        provider: agent.provider.clone(),
        model: agent.model.clone(),
        reasoning_effort: agent.reasoning_effort.clone(),
        peer_agents_enabled: agent.peer_agents_enabled,
        quality_feedback_injection_enabled: true,
        memory: agent.memory.clone(),
    })
    .unwrap_or_else(|| {
        format!(
            "summary_agent `{}` prompt could not be loaded. Keep the summary persona and produce a neutral meeting record.",
            resolved_summary_agent
        )
    })
}
