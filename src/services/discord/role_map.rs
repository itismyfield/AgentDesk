use std::fs;

use poise::serenity_prelude::ChannelId;

use super::meeting::{MeetingAgentConfig, MeetingConfig, SummaryAgentConfig, SummaryAgentRule};
use super::runtime_store::role_map_path;
use super::settings::{
    MemoryConfigOverride, PeerAgentInfo, RegisteredChannelBinding, RoleBinding,
    resolve_memory_settings,
};
use crate::services::provider::ProviderKind;

/// Expand `~` or `~/` prefix to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if path == "~" {
            return home.display().to_string();
        }
        if path.starts_with("~/") {
            return format!("{}{}", home.display(), &path[1..]);
        }
    }
    path.to_string()
}

fn load_role_map_json() -> Option<serde_json::Value> {
    let path = role_map_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn parse_role_binding(value: &serde_json::Value) -> Option<RoleBinding> {
    let obj = value.as_object()?;
    let role_id = obj.get("roleId")?.as_str()?.to_string();
    let prompt_file = expand_tilde(obj.get("promptFile")?.as_str()?);
    let provider = obj
        .get("provider")
        .and_then(|v| v.as_str())
        .and_then(ProviderKind::from_str);
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let reasoning_effort = obj
        .get("reasoningEffort")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let peer_agents_enabled = obj
        .get("peerAgents")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let memory_override = obj.get("memory").and_then(|raw| {
        serde_json::from_value::<MemoryConfigOverride>(raw.clone())
            .map_err(|err| {
                tracing::warn!("  [memory] Warning: invalid role_map memory block: {err}");
                err
            })
            .ok()
    });
    Some(RoleBinding {
        role_id,
        prompt_file,
        provider,
        model,
        reasoning_effort,
        peer_agents_enabled,
        memory: resolve_memory_settings(None, memory_override.as_ref()),
    })
}

fn json_string_vec(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn json_string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

#[derive(Clone, Copy)]
struct MeetingAgentFallbackProfile {
    display_name: &'static str,
    keywords: &'static [&'static str],
    domain_summary: &'static str,
    strengths: &'static [&'static str],
    task_types: &'static [&'static str],
    anti_signals: &'static [&'static str],
    provider_hint: &'static str,
}

fn meeting_agent_fallback_profile(role_id: &str) -> Option<MeetingAgentFallbackProfile> {
    match role_id {
        "openclaw-brain" => Some(MeetingAgentFallbackProfile {
            display_name: "똑똑이 | Brain",
            keywords: &["analysis", "reasoning", "architecture"],
            domain_summary: "복잡한 문제를 구조화하고 핵심 가설을 정리하는 분석 전문가",
            strengths: &["deep reasoning", "system thinking", "tradeoff analysis"],
            task_types: &["analysis", "architecture review", "decision support"],
            anti_signals: &["lightweight chit-chat"],
            provider_hint: "codex",
        }),
        "openclaw-realtor" => Some(MeetingAgentFallbackProfile {
            display_name: "복덕이 | Realtor",
            keywords: &["real estate", "housing", "location"],
            domain_summary: "부동산 입지, 매수·임대 판단, 실거주 조건 비교를 다루는 전문가",
            strengths: &["location review", "pricing sanity-check", "buyer needs fit"],
            task_types: &["market comparison", "housing recommendation", "risk review"],
            anti_signals: &["software implementation"],
            provider_hint: "codex",
        }),
        "openclaw-macro" => Some(MeetingAgentFallbackProfile {
            display_name: "흐름이 | Macro",
            keywords: &["macro", "economy", "scenario"],
            domain_summary: "거시지표와 외부 환경 변화를 바탕으로 시나리오와 우선순위를 정리하는 전문가",
            strengths: &["scenario planning", "signal triage", "priority framing"],
            task_types: &["macro analysis", "risk planning", "strategy framing"],
            anti_signals: &["pixel-perfect UI"],
            provider_hint: "codex",
        }),
        "openclaw-qa" => Some(MeetingAgentFallbackProfile {
            display_name: "큐에이 | QA",
            keywords: &["qa", "testing", "regression"],
            domain_summary: "회귀 위험, 검증 포인트, 테스트 설계를 우선적으로 점검하는 품질 전문가",
            strengths: &["regression analysis", "bug triage", "acceptance coverage"],
            task_types: &["test planning", "risk review", "quality gate"],
            anti_signals: &["creative copywriting"],
            provider_hint: "codex",
        }),
        "openclaw-coder" => Some(MeetingAgentFallbackProfile {
            display_name: "코딩이 | Coder",
            keywords: &["code", "implementation", "bugfix"],
            domain_summary: "구현 방법, 코드 변경 범위, 디버깅 경로를 구체화하는 엔지니어링 전문가",
            strengths: &["implementation planning", "debugging", "refactoring"],
            task_types: &["coding", "bug fix", "technical design"],
            anti_signals: &["non-technical lifestyle advice"],
            provider_hint: "codex",
        }),
        "openclaw-maker" => Some(MeetingAgentFallbackProfile {
            display_name: "뚝딱이 | Maker",
            keywords: &["automation", "prototype", "tools"],
            domain_summary: "빠른 프로토타입과 자동화 흐름을 설계하는 제작형 전문가",
            strengths: &["rapid prototyping", "tooling", "automation"],
            task_types: &["prototype build", "workflow automation", "integration"],
            anti_signals: &["long-form policy debate"],
            provider_hint: "codex",
        }),
        "openclaw-baby" => Some(MeetingAgentFallbackProfile {
            display_name: "쪽쪽이 | Baby",
            keywords: &["parenting", "baby", "routine"],
            domain_summary: "육아 루틴, 발달 관찰, 안전 수칙을 중심으로 조언하는 육아 전문가",
            strengths: &["routine guidance", "development check", "safety messaging"],
            task_types: &["parenting advice", "care planning", "safety review"],
            anti_signals: &["financial trading"],
            provider_hint: "codex",
        }),
        "openclaw-designer" => Some(MeetingAgentFallbackProfile {
            display_name: "기획이 | Designer",
            keywords: &["product", "ux", "spec"],
            domain_summary: "제품 구조, UX 흐름, 요구사항 문장을 정리하는 기획·디자인 전문가",
            strengths: &["ux framing", "spec writing", "information architecture"],
            task_types: &["product planning", "ux review", "requirement design"],
            anti_signals: &["low-level runtime debugging"],
            provider_hint: "codex",
        }),
        "openclaw-main" => Some(MeetingAgentFallbackProfile {
            display_name: "대복이 | Main",
            keywords: &["coordination", "decision", "synthesis"],
            domain_summary: "여러 관점을 엮어 최종 결정과 실행 우선순위를 정리하는 조율 전문가",
            strengths: &["synthesis", "decision framing", "coordination"],
            task_types: &[
                "meeting synthesis",
                "priority setting",
                "final recommendation",
            ],
            anti_signals: &["narrow single-domain drilldown"],
            provider_hint: "codex",
        }),
        "openclaw-gamer" => Some(MeetingAgentFallbackProfile {
            display_name: "겜돌이 | Gamer",
            keywords: &["game", "balance", "content"],
            domain_summary: "게임 시스템, 밸런스, 플레이 감각을 중심으로 검토하는 게임 전문가",
            strengths: &["gameplay review", "balance analysis", "content feedback"],
            task_types: &["game design", "balance review", "player experience"],
            anti_signals: &["legal compliance"],
            provider_hint: "codex",
        }),
        "qwen" => Some(MeetingAgentFallbackProfile {
            display_name: "qwen",
            keywords: &["analysis", "summary", "generalist"],
            domain_summary: "범용 분석과 요약, 대안 비교에 강한 일반 목적 전문가",
            strengths: &["broad analysis", "summarization", "alternative comparison"],
            task_types: &["analysis", "brainstorming", "synthesis"],
            anti_signals: &["repo mutation"],
            provider_hint: "qwen",
        }),
        "openclaw-crypto" => Some(MeetingAgentFallbackProfile {
            display_name: "코인이 | Crypto",
            keywords: &["crypto", "market", "onchain"],
            domain_summary: "가상자산 시장 흐름, 리스크, 온체인 시그널을 다루는 크립토 전문가",
            strengths: &["market reading", "risk framing", "narrative tracking"],
            task_types: &[
                "crypto analysis",
                "portfolio risk review",
                "market commentary",
            ],
            anti_signals: &["medical advice"],
            provider_hint: "codex",
        }),
        "openclaw-foodie" => Some(MeetingAgentFallbackProfile {
            display_name: "맛돌이 | Foodie",
            keywords: &["food", "restaurant", "menu"],
            domain_summary: "식당 선택, 메뉴 평가, 방문 동선을 구체적으로 제안하는 미식 전문가",
            strengths: &[
                "restaurant filtering",
                "menu recommendation",
                "visit planning",
            ],
            task_types: &["restaurant recommendation", "taste review", "trip planning"],
            anti_signals: &["source code debugging"],
            provider_hint: "codex",
        }),
        _ => None,
    }
}

fn meeting_agent_from_json(agent: &serde_json::Value) -> Option<MeetingAgentConfig> {
    let role_id = agent
        .get("role_id")
        .or_else(|| agent.get("roleId"))?
        .as_str()?;
    let fallback = meeting_agent_fallback_profile(role_id);
    let display_name = agent
        .get("display_name")
        .or_else(|| agent.get("displayName"))
        .and_then(|v| v.as_str())
        .or_else(|| fallback.map(|profile| profile.display_name))
        .unwrap_or(role_id);
    let prompt_file = expand_tilde(
        agent
            .get("prompt_file")
            .or_else(|| agent.get("promptFile"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let provider_raw = json_string_field(agent, &["provider"]);
    let provider_hint = json_string_field(agent, &["provider_hint", "providerHint", "provider"])
        .or_else(|| fallback.map(|profile| profile.provider_hint.to_string()));
    let provider = provider_raw
        .as_deref()
        .and_then(ProviderKind::from_str)
        .or_else(|| fallback.and_then(|profile| ProviderKind::from_str(profile.provider_hint)));
    let model = json_string_field(agent, &["model"]);
    let reasoning_effort = json_string_field(agent, &["reasoning_effort", "reasoningEffort"]);
    let workspace = json_string_field(agent, &["workspace"]).map(|value| expand_tilde(&value));
    let peer_agents_enabled = agent
        .get("peerAgents")
        .or_else(|| agent.get("peer_agents"))
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let memory_override = agent.get("memory").and_then(|raw| {
        serde_json::from_value::<MemoryConfigOverride>(raw.clone())
            .map_err(|err| {
                tracing::warn!(
                    "  [memory] Warning: invalid meeting.available_agents memory block: {err}"
                );
                err
            })
            .ok()
    });

    Some(MeetingAgentConfig {
        role_id: role_id.to_string(),
        display_name: display_name.to_string(),
        keywords: {
            let explicit = json_string_vec(agent, "keywords");
            if explicit.is_empty() {
                fallback
                    .map(|profile| {
                        profile
                            .keywords
                            .iter()
                            .map(|value| (*value).to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                explicit
            }
        },
        prompt_file,
        domain_summary: json_string_field(agent, &["domain_summary", "domainSummary"])
            .or_else(|| fallback.map(|profile| profile.domain_summary.to_string())),
        strengths: {
            let explicit = json_string_vec(agent, "strengths");
            if explicit.is_empty() {
                fallback
                    .map(|profile| {
                        profile
                            .strengths
                            .iter()
                            .map(|value| (*value).to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                explicit
            }
        },
        task_types: {
            let explicit = json_string_vec(agent, "task_types")
                .into_iter()
                .chain(json_string_vec(agent, "taskTypes"))
                .collect::<Vec<_>>();
            if explicit.is_empty() {
                fallback
                    .map(|profile| {
                        profile
                            .task_types
                            .iter()
                            .map(|value| (*value).to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                explicit
            }
        },
        anti_signals: {
            let explicit = json_string_vec(agent, "anti_signals")
                .into_iter()
                .chain(json_string_vec(agent, "antiSignals"))
                .collect::<Vec<_>>();
            if explicit.is_empty() {
                fallback
                    .map(|profile| {
                        profile
                            .anti_signals
                            .iter()
                            .map(|value| (*value).to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                explicit
            }
        },
        provider_hint,
        provider,
        model,
        reasoning_effort,
        workspace,
        peer_agents_enabled,
        memory: resolve_memory_settings(None, memory_override.as_ref()),
    })
}

fn collect_role_map_binding_agents(json: &serde_json::Value) -> Vec<MeetingAgentConfig> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for section in ["byChannelId", "byChannelName"] {
        let Some(bindings) = json.get(section).and_then(|v| v.as_object()) else {
            continue;
        };
        for binding in bindings.values() {
            let Some(agent) = meeting_agent_from_json(binding) else {
                continue;
            };
            if seen.insert(agent.role_id.clone()) {
                result.push(agent);
            }
        }
    }
    result.sort_by(|a, b| a.role_id.cmp(&b.role_id));
    result
}

fn fallback_enabled(json: &serde_json::Value) -> bool {
    json.get("fallbackByChannelName")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn entry_matches_channel_id(entry: &serde_json::Value, channel_id: ChannelId) -> bool {
    match entry
        .get("channelId")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(explicit_channel_id) => explicit_channel_id == channel_id.get().to_string(),
        None => true,
    }
}

pub(super) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    let json = load_role_map_json()?;

    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        let key = channel_id.get().to_string();
        if let Some(binding) = by_id.get(&key).and_then(parse_role_binding) {
            return Some(binding);
        }
    }

    if !fallback_enabled(&json) {
        return None;
    }

    let cname = channel_name?;
    let by_name = json.get("byChannelName").and_then(|v| v.as_object())?;
    let entry = by_name.get(cname)?;
    entry_matches_channel_id(entry, channel_id).then_some(())?;
    parse_role_binding(entry)
}

pub(super) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    let json = load_role_map_json()?;

    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        let key = channel_id.get().to_string();
        if let Some(entry) = by_id.get(&key) {
            if let Some(ws) = entry.get("workspace").and_then(|v| v.as_str()) {
                return Some(expand_tilde(ws));
            }
        }
    }

    if !fallback_enabled(&json) {
        return None;
    }

    let cname = channel_name?;
    let by_name = json.get("byChannelName").and_then(|v| v.as_object())?;
    let entry = by_name.get(cname)?;
    entry_matches_channel_id(entry, channel_id).then_some(())?;
    entry
        .get("workspace")
        .and_then(|v| v.as_str())
        .map(|s| expand_tilde(s))
}

pub(super) fn load_shared_prompt_path() -> Option<String> {
    let json = load_role_map_json()?;
    json.get("sharedPromptFile")
        .and_then(|v| v.as_str())
        .map(|s| expand_tilde(s))
        .or_else(|| {
            let root = crate::config::runtime_root()?;
            let canonical = crate::runtime_layout::shared_prompt_path(&root);
            canonical
                .exists()
                .then(|| canonical.display().to_string())
                .or_else(|| {
                    let legacy = crate::runtime_layout::config_dir(&root).join("_shared.md");
                    legacy.exists().then(|| legacy.display().to_string())
                })
        })
}

/// Check if a role_id exists in any channel binding in role_map.json.
pub(super) fn is_known_agent(role_id: &str) -> bool {
    let Some(json) = load_role_map_json() else {
        return false;
    };
    // Check byChannelId
    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        for entry in by_id.values() {
            if entry.get("roleId").and_then(|v| v.as_str()) == Some(role_id) {
                return true;
            }
        }
    }
    // Check byChannelName
    if let Some(by_name) = json.get("byChannelName").and_then(|v| v.as_object()) {
        for entry in by_name.values() {
            if entry.get("roleId").and_then(|v| v.as_str()) == Some(role_id) {
                return true;
            }
        }
    }
    false
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let Some(json) = load_role_map_json() else {
        return Vec::new();
    };
    let agents = match json
        .get("meeting")
        .and_then(|meeting| meeting.get("available_agents"))
    {
        Some(value) => value
            .as_array()
            .map(|available| {
                available
                    .iter()
                    .filter_map(meeting_agent_from_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        None => collect_role_map_binding_agents(&json),
    };

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for agent in agents {
        if !seen.insert(agent.role_id.clone()) {
            continue;
        }

        result.push(PeerAgentInfo {
            role_id: agent.role_id,
            display_name: agent.display_name,
            keywords: agent.keywords,
        });
    }

    result
}

fn parse_summary_agent_config(value: &serde_json::Value) -> Option<SummaryAgentConfig> {
    // Backward-compatible: accept plain string
    if let Some(s) = value.as_str() {
        return Some(SummaryAgentConfig::Static(s.to_string()));
    }

    // Rule-based: { "rules": [...], "default": "..." }
    let obj = value.as_object()?;
    let default_agent = obj.get("default")?.as_str()?.to_string();
    let rules_arr = obj.get("rules").and_then(|v| v.as_array());
    let mut rules = Vec::new();
    if let Some(arr) = rules_arr {
        for rule in arr {
            let agent = rule.get("agent").and_then(|v| v.as_str());
            let keywords = rule.get("keywords").and_then(|v| v.as_array()).map(|kws| {
                kws.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });
            if let (Some(agent), Some(keywords)) = (agent, keywords) {
                rules.push(SummaryAgentRule {
                    keywords,
                    agent: agent.to_string(),
                });
            }
        }
    }
    Some(SummaryAgentConfig::Dynamic {
        rules,
        default: default_agent,
    })
}

pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    let json = load_role_map_json()?;
    let meeting = json.get("meeting")?;

    let channel_name = meeting.get("channel_name")?.as_str()?.to_string();
    let max_rounds = meeting
        .get("max_rounds")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as u32;
    let max_participants = meeting
        .get("max_participants")
        .or_else(|| meeting.get("maxParticipants"))
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as usize;
    let summary_agent = parse_summary_agent_config(meeting.get("summary_agent")?)?;

    let available_agents = match meeting.get("available_agents") {
        Some(value) => value
            .as_array()
            .map(|agents_arr| {
                agents_arr
                    .iter()
                    .filter_map(meeting_agent_from_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        None => collect_role_map_binding_agents(&json),
    };

    Some(MeetingConfig {
        channel_name,
        max_rounds,
        max_participants,
        summary_agent,
        available_agents,
    })
}

pub(super) fn list_registered_channel_bindings() -> Vec<RegisteredChannelBinding> {
    let Some(json) = load_role_map_json() else {
        return Vec::new();
    };

    let mut bindings = Vec::new();
    if let Some(by_id) = json.get("byChannelId").and_then(|value| value.as_object()) {
        for (channel_id_raw, entry) in by_id {
            let Ok(channel_id) = channel_id_raw.parse::<u64>() else {
                continue;
            };
            let Some(binding) = parse_role_binding(entry) else {
                continue;
            };
            let Some(owner_provider) = binding.provider.filter(ProviderKind::is_supported) else {
                continue;
            };
            bindings.push(RegisteredChannelBinding {
                channel_id,
                owner_provider,
                fallback_name: None,
            });
        }
    }

    bindings.sort_by_key(|binding| binding.channel_id);
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Mem0EnvGuard {
        prev_api_key: Option<std::ffi::OsString>,
        prev_base_url: Option<std::ffi::OsString>,
    }

    impl Mem0EnvGuard {
        fn install() -> Self {
            crate::services::memory::reset_backend_health_for_tests();
            let prev_api_key = std::env::var_os("MEM0_API_KEY");
            let prev_base_url = std::env::var_os("MEM0_BASE_URL");
            unsafe {
                std::env::set_var("MEM0_API_KEY", "test-key");
                std::env::set_var("MEM0_BASE_URL", "http://mem0.local");
            }
            Self {
                prev_api_key,
                prev_base_url,
            }
        }
    }

    impl Drop for Mem0EnvGuard {
        fn drop(&mut self) {
            match self.prev_api_key.take() {
                Some(value) => unsafe { std::env::set_var("MEM0_API_KEY", value) },
                None => unsafe { std::env::remove_var("MEM0_API_KEY") },
            }
            match self.prev_base_url.take() {
                Some(value) => unsafe { std::env::set_var("MEM0_BASE_URL", value) },
                None => unsafe { std::env::remove_var("MEM0_BASE_URL") },
            }
            crate::services::memory::reset_backend_health_for_tests();
        }
    }

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(&TempDir),
    {
        let _guard = super::super::runtime_store::lock_test_env();
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(".adk");
        std::fs::create_dir_all(&root).unwrap();
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f(&temp);
        match prev {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    fn write_role_map(dir: &std::path::Path, content: &str) {
        let settings_dir = dir.join(".adk").join("config");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(settings_dir.join("role_map.json"), content).unwrap();
    }

    #[test]
    fn test_resolve_role_binding_reads_memory_block_from_role_map() {
        with_temp_root(|temp_home: &TempDir| {
            let _mem0_env = Mem0EnvGuard::install();
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {
    "123": {
      "roleId": "codex",
      "promptFile": "~/prompts/codex.md",
      "provider": "codex",
      "memory": {
        "backend": "mem0",
        "recall_timeout_ms": 50,
        "capture_timeout_ms": 8000,
        "mem0": {
          "profile": "strict",
          "ingestion": {
            "infer": true,
            "custom_instructions": "Remember deployment facts"
          }
        }
      }
    }
  }
}"#,
            );

            let binding = resolve_role_binding(ChannelId::new(123), None).unwrap();
            assert_eq!(
                binding.memory.backend,
                super::super::settings::MemoryBackendKind::Mem0
            );
            assert_eq!(binding.memory.recall_timeout_ms, 100);
            assert_eq!(binding.memory.capture_timeout_ms, 8000);
            assert_eq!(binding.memory.mem0.profile, "strict");
            assert_eq!(binding.memory.mem0.ingestion.infer, Some(true));
            assert_eq!(
                binding.memory.mem0.ingestion.custom_instructions.as_deref(),
                Some("Remember deployment facts")
            );
        });
    }

    #[test]
    fn test_resolve_role_binding_skips_by_name_entry_when_channel_id_mismatches() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelName": {
    "adk-cc": {
      "channelId": "1484070499783803081",
      "promptFile": "~/prompts/project-agentdesk.md",
      "provider": "claude",
      "roleId": "project-agentdesk",
      "workspace": "~/workspaces/agentdesk"
    }
  },
  "fallbackByChannelName": {
    "enabled": true
  }
}"#,
            );

            assert!(
                resolve_role_binding(ChannelId::new(1479671298497183835), Some("adk-cc")).is_none()
            );
            assert!(
                resolve_workspace(ChannelId::new(1479671298497183835), Some("adk-cc")).is_none()
            );
        });
    }

    #[test]
    fn test_load_meeting_config_reads_metadata_and_max_participants() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "meeting": {
    "channel_name": "meeting",
    "max_participants": 4,
    "summary_agent": "qwen",
    "available_agents": [
      {
        "role_id": "qwen",
        "display_name": "Qwen Specialist",
        "prompt_file": "~/prompts/qwen.md",
        "keywords": ["analysis"],
        "domain_summary": "Deep reasoning specialist",
        "strengths": ["long-context synthesis"],
        "task_types": ["analysis"],
        "anti_signals": ["short notification"],
        "provider_hint": "qwen"
      }
    ]
  }
}"#,
            );

            let config = load_meeting_config().expect("meeting config should load");
            assert_eq!(config.max_participants, 4);
            assert_eq!(config.available_agents.len(), 1);
            let qwen = &config.available_agents[0];
            assert_eq!(qwen.role_id, "qwen");
            assert_eq!(qwen.display_name, "Qwen Specialist");
            assert!(qwen.prompt_file.ends_with("/prompts/qwen.md"));
            assert_eq!(
                qwen.domain_summary.as_deref(),
                Some("Deep reasoning specialist")
            );
            assert_eq!(qwen.strengths, vec!["long-context synthesis".to_string()]);
            assert_eq!(qwen.task_types, vec!["analysis".to_string()]);
            assert_eq!(qwen.anti_signals, vec!["short notification".to_string()]);
            assert_eq!(qwen.provider_hint.as_deref(), Some("qwen"));
        });
    }

    #[test]
    fn test_load_shared_prompt_path_falls_back_to_canonical_runtime_path() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(temp_home.path(), r#"{"byChannelId": {}}"#);
            let canonical = temp_home
                .path()
                .join(".adk")
                .join("config")
                .join("agents")
                .join("_shared.prompt.md");
            std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            std::fs::write(&canonical, "# shared").unwrap();

            let shared = load_shared_prompt_path().expect("shared prompt path");
            assert_eq!(std::path::Path::new(&shared), canonical);
        });
    }

    #[test]
    fn test_load_meeting_config_empty_available_agents_stays_empty() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {
    "123": {
      "roleId": "gemini",
      "displayName": "Gemini Specialist",
      "promptFile": "~/prompts/gemini.md",
      "provider": "gemini"
    }
  },
  "meeting": {
    "channel_name": "meeting",
    "summary_agent": "gemini",
    "available_agents": []
  }
}"#,
            );

            let config = load_meeting_config().expect("meeting config should load");
            assert!(config.available_agents.is_empty());
        });
    }

    #[test]
    fn test_load_meeting_config_fallback_agents_inherit_known_specialist_metadata() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {
    "123": {
      "roleId": "openclaw-coder",
      "promptFile": "~/prompts/coder.md"
    }
  },
  "meeting": {
    "channel_name": "meeting",
    "summary_agent": "openclaw-coder"
  }
}"#,
            );

            let config = load_meeting_config().expect("meeting config should load");
            assert_eq!(config.available_agents.len(), 1);
            let coder = &config.available_agents[0];
            assert_eq!(coder.role_id, "openclaw-coder");
            assert_eq!(coder.display_name, "코딩이 | Coder");
            assert_eq!(
                coder.domain_summary.as_deref(),
                Some("구현 방법, 코드 변경 범위, 디버깅 경로를 구체화하는 엔지니어링 전문가")
            );
            assert!(
                coder
                    .strengths
                    .contains(&"implementation planning".to_string())
            );
            assert!(coder.task_types.contains(&"coding".to_string()));
            assert_eq!(coder.provider_hint.as_deref(), Some("codex"));
            assert_eq!(coder.provider, Some(ProviderKind::Codex));
        });
    }
}
