use super::*;
use serde::Deserialize;

/// POST /api/queue/request-generate (#2126)
///
/// Dashboard-facing convenience endpoint. The dashboard asks the backend to
/// dispatch a standardized "generate this queue, you decide order/phase/gate"
/// instruction to an agent's Discord channel. The agent then judges the
/// payload, calls `/api/queue/generate` itself, and reports back.
///
/// Putting the instruction text and channel routing here keeps a single
/// source of truth: when the instruction wording or the contract changes,
/// only this file moves, not every dashboard build.
#[derive(Debug, Deserialize)]
pub struct RequestGenerateBody {
    pub repo: String,
    pub agent_id: String,
    pub issue_numbers: Vec<i64>,
    /// Optional restriction on which phase-gate kinds the agent may use
    /// when building entries. When omitted, every kind in
    /// `/api/queue/phase-gates/catalog` is implicitly allowed.
    pub allowed_gate_kinds: Option<Vec<String>>,
    /// Reserved for future force-cancel semantics. Accepted from callers
    /// for forward compatibility but currently has no effect and is not
    /// echoed in the response — do not depend on it round-tripping.
    #[serde(default)]
    pub force: Option<bool>,
}

pub async fn request_generate(
    State(state): State<AppState>,
    body: Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let body: RequestGenerateBody = match serde_json::from_value(body.0) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid request body: {error}") })),
            );
        }
    };

    let repo = body.repo.trim();
    if repo.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "repo is required" })),
        );
    }
    let agent_id = body.agent_id.trim();
    if agent_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "agent_id is required" })),
        );
    }
    if body.issue_numbers.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "issue_numbers must be non-empty" })),
        );
    }

    let allowed_gate_kinds = match validate_allowed_gate_kinds(body.allowed_gate_kinds.as_deref()) {
        Ok(kinds) => kinds,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
        }
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let instruction = build_request_generate_instruction(&RequestGenerateInput {
        repo,
        agent_id,
        issue_numbers: &body.issue_numbers,
        allowed_gate_kinds: allowed_gate_kinds.as_deref(),
        request_id: &request_id,
    });

    let Some(ref registry) = state.health_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "Discord not available (standalone mode); cannot dispatch request-generate",
            })),
        );
    };

    let send_body = json!({
        "role_id": agent_id,
        "message": instruction,
        "mode": "announce",
    })
    .to_string();
    let (status_str, response_body) = crate::services::discord::health::handle_send_to_agent(
        registry,
        None,
        state.pg_pool_ref(),
        &send_body,
    )
    .await;

    let send_json: serde_json::Value = serde_json::from_str(&response_body)
        .unwrap_or_else(|_| json!({"ok": false, "error": "internal"}));

    if !status_str.starts_with("200") {
        let code = status_str
            .split_whitespace()
            .next()
            .and_then(|raw| raw.parse::<u16>().ok())
            .and_then(|raw| StatusCode::from_u16(raw).ok())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return (code, Json(send_json));
    }

    let channel_id = send_json
        .get("channel_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let dispatched_at = send_json
        .get("sent_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    crate::auto_queue_log!(
        info,
        "request_generate.dispatched",
        AutoQueueLogContext::new().agent(agent_id),
        "request_id={} repo={} issues={:?} allowed_gate_kinds={:?}",
        request_id,
        repo,
        body.issue_numbers,
        allowed_gate_kinds
    );

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "request_id": request_id,
            "target": format!("agent:{}", agent_id),
            "channel_id": channel_id,
            "dispatched_at": dispatched_at,
            "instruction_preview": preview_instruction(&instruction),
        })),
    )
}

fn validate_allowed_gate_kinds(kinds: Option<&[String]>) -> Result<Option<Vec<String>>, String> {
    let Some(kinds) = kinds else { return Ok(None) };
    if kinds.is_empty() {
        return Ok(None);
    }
    let mut normalized = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let trimmed = kind.trim();
        if trimmed.is_empty() {
            return Err("allowed_gate_kinds entries must not be empty".to_string());
        }
        if !super::phase_gate_catalog::is_valid_phase_gate_kind(trimmed) {
            return Err(format!(
                "unknown phase_gate_kind '{trimmed}' (see GET /api/queue/phase-gates/catalog)"
            ));
        }
        if !normalized.iter().any(|existing: &String| existing == trimmed) {
            normalized.push(trimmed.to_string());
        }
    }
    Ok(Some(normalized))
}

pub(super) struct RequestGenerateInput<'a> {
    pub repo: &'a str,
    pub agent_id: &'a str,
    pub issue_numbers: &'a [i64],
    pub allowed_gate_kinds: Option<&'a [String]>,
    pub request_id: &'a str,
}

/// Build the self-contained instruction the agent receives in its Discord
/// channel. Self-contained on purpose: a freshly-started agent with no prior
/// turn context must still be able to act on this message alone (#2126).
///
/// When `allowed_gate_kinds` is set, the call-schema example and the guide
/// reference the restricted vocabulary so a literal-minded agent doesn't
/// echo the catalog default (`pr-confirm`) into a queue that was meant to be
/// deploy-only. `/api/queue/generate` doesn't re-check the restriction
/// today, so the prompt is the only place where the constraint is conveyed.
pub(super) fn build_request_generate_instruction(input: &RequestGenerateInput<'_>) -> String {
    let issues = input
        .issue_numbers
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (allowed_kinds_line, example_kind, kind_guide_line) = match input.allowed_gate_kinds {
        Some(kinds) if !kinds.is_empty() => {
            let formatted = kinds
                .iter()
                .map(|kind| format!("\"{kind}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let example = kinds[0].clone();
            let guide = format!(
                "- phase_gate_kind: 반드시 allowed_gate_kinds([{formatted}]) 중에서만 선택 (이 목록 밖의 id는 사용 금지)"
            );
            (
                format!("allowed_gate_kinds: [{formatted}]\n"),
                example,
                guide,
            )
        }
        _ => (
            String::new(),
            super::phase_gate_catalog::DEFAULT_PHASE_GATE_KIND.to_string(),
            format!(
                "- phase_gate_kind: GET /api/queue/phase-gates/catalog의 id 중 하나, 미지정 시 default_kind ({})",
                super::phase_gate_catalog::DEFAULT_PHASE_GATE_KIND
            ),
        ),
    };
    format!(
        "[자동큐 생성 의뢰] (request_id: {request_id})\n\
         repo: {repo}\n\
         issues: [{issues}]\n\
         {allowed_kinds_line}\n\
         다음 절차로 처리:\n\
         1) 각 이슈를 gh로 직접 조회하여 본문/라벨/관련 의존성 파악\n\
         2) 순서·thread_group(병렬 가능 단위)·batch_phase(직렬 페이즈)·phase_gate_kind 결정\n\
         3) /api/queue/generate 호출 (스키마 아래)\n\n\
         호출 스키마:\n\
         POST /api/queue/generate\n\
         {{\n\
         \u{20}\u{20}\"repo\": \"{repo}\",\n\
         \u{20}\u{20}\"agent_id\": \"{agent_id}\",\n\
         \u{20}\u{20}\"entries\": [\n\
         \u{20}\u{20}\u{20}\u{20}{{ \"issue_number\": N, \"batch_phase\": 0, \"thread_group\": 1, \"phase_gate_kind\": \"{example_kind}\" }},\n\
         \u{20}\u{20}\u{20}\u{20}...\n\
         \u{20}\u{20}],\n\
         \u{20}\u{20}\"max_concurrent_threads\": <옵션>,\n\
         \u{20}\u{20}\"review_mode\": \"enabled\"\n\
         }}\n\n\
         가이드:\n\
         - entries는 실행 순서대로 정렬\n\
         - thread_group: 동시 실행 가능한 카드는 같은 group, 직렬은 다른 group\n\
         - batch_phase: 페이즈 게이트 사이에 있는 카드는 같은 phase\n\
         {kind_guide_line}\n\
         - 자체 판단 후 즉시 호출, 완료 시 run_id를 본 채널에 보고",
        request_id = input.request_id,
        repo = input.repo,
        issues = issues,
        agent_id = input.agent_id,
        allowed_kinds_line = allowed_kinds_line,
        example_kind = example_kind,
        kind_guide_line = kind_guide_line,
    )
}

fn preview_instruction(text: &str) -> String {
    let limit = 200;
    if text.len() <= limit {
        return text.to_string();
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_contains_request_id_repo_and_issues() {
        let issues = vec![2120i64, 2121, 2122];
        let allowed = vec!["pr-confirm".to_string(), "deploy-gate".to_string()];
        let text = build_request_generate_instruction(&RequestGenerateInput {
            repo: "itismyfield/AgentDesk",
            agent_id: "project-agentdesk",
            issue_numbers: &issues,
            allowed_gate_kinds: Some(&allowed),
            request_id: "req-abc",
        });
        assert!(text.contains("req-abc"));
        assert!(text.contains("itismyfield/AgentDesk"));
        assert!(text.contains("#2120"));
        assert!(text.contains("#2121"));
        assert!(text.contains("#2122"));
        assert!(text.contains("allowed_gate_kinds: [\"pr-confirm\", \"deploy-gate\"]"));
        assert!(text.contains("/api/queue/generate"));
        assert!(text.contains("phase_gate_kind"));
    }

    #[test]
    fn instruction_omits_allowed_line_when_none() {
        let issues = vec![1i64];
        let text = build_request_generate_instruction(&RequestGenerateInput {
            repo: "r",
            agent_id: "a",
            issue_numbers: &issues,
            allowed_gate_kinds: None,
            request_id: "r1",
        });
        assert!(!text.contains("allowed_gate_kinds:"));
    }

    /// Regression for codex review P1 on #2126: when allowed_gate_kinds
    /// excludes the catalog default (`pr-confirm`), the synthesised call
    /// example must not bleed the default back into the prompt and the
    /// guide must explicitly forbid kinds outside the restriction.
    #[test]
    fn instruction_example_kind_respects_allowed_restriction() {
        let issues = vec![42i64];
        let allowed = vec!["deploy-gate".to_string()];
        let text = build_request_generate_instruction(&RequestGenerateInput {
            repo: "r",
            agent_id: "a",
            issue_numbers: &issues,
            allowed_gate_kinds: Some(&allowed),
            request_id: "r1",
        });
        assert!(
            text.contains("\"phase_gate_kind\": \"deploy-gate\""),
            "call example should use the first allowed kind: {text}"
        );
        assert!(
            !text.contains("\"phase_gate_kind\": \"pr-confirm\""),
            "pr-confirm must not appear when restricted to deploy-gate: {text}"
        );
        assert!(
            text.contains("반드시 allowed_gate_kinds"),
            "guide must explicitly require the restriction: {text}"
        );
    }

    #[test]
    fn validate_allowed_gate_kinds_accepts_catalog_values() {
        let input = vec!["pr-confirm".to_string(), "deploy-gate".to_string()];
        let validated = validate_allowed_gate_kinds(Some(&input)).expect("valid");
        assert_eq!(validated.as_deref().map(|v| v.len()), Some(2));
    }

    #[test]
    fn validate_allowed_gate_kinds_rejects_unknown() {
        let input = vec!["pr-confirm".to_string(), "ship-it".to_string()];
        let error = validate_allowed_gate_kinds(Some(&input)).unwrap_err();
        assert!(error.contains("ship-it"));
    }

    #[test]
    fn validate_allowed_gate_kinds_dedups() {
        let input = vec!["pr-confirm".to_string(), "pr-confirm".to_string()];
        let validated = validate_allowed_gate_kinds(Some(&input)).expect("valid");
        assert_eq!(validated.as_deref(), Some(&["pr-confirm".to_string()][..]));
    }

    #[test]
    fn validate_allowed_gate_kinds_none_passthrough() {
        assert!(validate_allowed_gate_kinds(None).unwrap().is_none());
        assert!(validate_allowed_gate_kinds(Some(&[])).unwrap().is_none());
    }
}
