
// ── Provider Check ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CheckProviderBody {
    pub provider: String,
}

/// POST /api/onboarding/check-provider
/// Checks if a CLI provider (claude/codex/gemini/opencode/qwen) is installed and authenticated.
pub async fn check_provider(body: CheckProviderBody) -> (StatusCode, Json<serde_json::Value>) {
    let cmd = match body.provider.as_str() {
        "claude" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "opencode" => "opencode",
        "qwen" => "qwen",
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "provider must be 'claude', 'codex', 'gemini', 'opencode', or 'qwen'"}),
                ),
            );
        }
    };

    // Resolve binary using the exact same provider-specific resolver as the runtime,
    // including known-path fallbacks (~/bin, /opt/homebrew/bin, etc.).
    // This ensures onboarding and actual launch always agree on availability.
    let resolution = {
        let provider = cmd.to_string();
        tokio::task::spawn_blocking(move || {
            crate::services::platform::resolve_provider_binary(&provider)
        })
        .await
        .ok()
    }
    .unwrap_or_else(|| crate::services::platform::resolve_provider_binary(cmd));
    let mut failure_kind = resolution.failure_kind.clone();

    let Some(bin_path) = resolution.resolved_path.clone() else {
        return (
            StatusCode::OK,
            Json(json!({
                "installed": false,
                "logged_in": false,
                "version": null,
                "path": null,
                "canonical_path": null,
                "source": null,
                "failure_kind": resolution.failure_kind,
                "attempts": resolution.attempts,
            })),
        );
    };

    // Get version using the resolved binary path (not bare command name)
    // so it works even when PATH doesn't contain the provider.
    let (version, probe_failure_kind) = {
        let resolution = resolution.clone();
        let bin_path = bin_path.clone();
        tokio::task::spawn_blocking(move || {
            crate::services::platform::probe_resolved_binary_version(&bin_path, &resolution)
        })
        .await
        .ok()
        .unwrap_or((None, Some("version_probe_spawn_failed".to_string())))
    };
    if failure_kind.is_none() {
        failure_kind = probe_failure_kind.clone();
    }

    // Check login (heuristic: config directory exists with content)
    let logged_in = if cmd == "opencode" {
        true
    } else {
        dirs::home_dir()
            .map(|home| {
                let config_dir = if cmd == "claude" {
                    home.join(".claude")
                } else if cmd == "codex" {
                    home.join(".codex")
                } else if cmd == "qwen" {
                    home.join(".qwen")
                } else {
                    home.join(".gemini")
                };
                config_dir.is_dir()
            })
            .unwrap_or(false)
    };

    (
        StatusCode::OK,
        Json(json!({
            "installed": true,
            "logged_in": logged_in,
            "version": version,
            "path": resolution.resolved_path,
            "canonical_path": resolution.canonical_path,
            "source": resolution.source,
            "failure_kind": failure_kind,
            "attempts": resolution.attempts,
        })),
    )
}

// ── AI Prompt Generation ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GeneratePromptBody {
    pub name: String,
    pub description: String,
    pub provider: Option<String>,
}

/// POST /api/onboarding/generate-prompt
/// Generates a system prompt for a custom agent using the local CLI.
pub async fn generate_prompt(body: GeneratePromptBody) -> (StatusCode, Json<serde_json::Value>) {
    let provider = body
        .provider
        .as_deref()
        .and_then(ProviderKind::from_str)
        .unwrap_or(ProviderKind::Claude);

    let instruction = format!(
        "다음 AI 에이전트의 시스템 프롬프트를 한국어로 작성해줘.\n\
         이름: {}\n설명: {}\n\n\
         에이전트의 역할, 핵심 능력, 소통 스타일을 포함해서 5-10줄로 작성해.\n\
         시스템 프롬프트 텍스트만 출력하고 다른 설명은 붙이지 마.",
        body.name, body.description
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        provider_exec::execute_simple(provider, instruction),
    )
    .await;

    if let Ok(Ok(text)) = result {
        if !text.trim().is_empty() {
            return (
                StatusCode::OK,
                Json(json!({ "prompt": text.trim(), "source": "ai" })),
            );
        }
    }

    // Fallback to template
    let fallback = format!(
        "당신은 '{name}'입니다. {desc}\n\n\
         ## 역할\n\
         - 위 설명에 맞는 업무를 수행합니다\n\
         - 사용자의 요청에 정확하고 친절하게 응답합니다\n\n\
         ## 소통 원칙\n\
         - 한국어로 소통합니다\n\
         - 간결하고 명확하게 답변합니다\n\
         - 필요시 확인 질문을 합니다",
        name = body.name,
        desc = body.description,
    );

    (
        StatusCode::OK,
        Json(json!({ "prompt": fallback, "source": "template" })),
    )
}
