use super::settings::{
    MemoryBackendKind, ResolvedMemorySettings, discord_token_hash, load_review_tuning_guidance,
    load_role_prompt, load_shared_prompt, load_shared_prompt_for_profile,
    render_peer_agent_guidance,
};
use super::*;
use crate::services::memory::{
    UNBOUND_MEMORY_ROLE_ID, resolve_memento_agent_id, resolve_memento_workspace,
    sanitize_memento_workspace_segment,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const CONTEXT_COMPRESSION_SECTION_ORDER: &str = "`Goal`, `Progress`, `Decisions`, `Files`, `Next`";
const STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE: &str =
    "[이전 결과 — 3줄 요약: cargo test failed in src/foo.rs because ...]";
const DISPATCH_CONTRACT_COMMON: &str = "- 구현이 불필요하고 현재 worktree에 tracked 변경이 전혀 없을 때만 응답 첫 줄에 `OUTCOME: noop`를 적고 근거를 설명한다.\n\
     - tracked 변경이 남아 있으면 noop를 사용하지 않는다.\n\
     - 관련 검증(`cargo build`, 좁은 테스트 등)을 직접 실행하고 결과를 최종 응답에 포함한다.\n\
     - `▶ Ready for input (type message + Enter)` 는 완료 마커가 아니다.";
const DIRECT_FIRST_COMPLETION_CONTRACT: &str = "- `merge_strategy_mode=direct-first` 기본 계약:\n\
     - origin/main 기준으로 worktree에 타 작업이 섞이지 않았는지 확인한다.\n\
     - 커밋 메시지는 `#<issue> <title>` 형식을 사용한다.\n\
     - 완료 조건은 `git commit` 뒤 `git push origin HEAD:main` 성공이다.\n\
     - `git push` 가 non-fast-forward, pre-push hook, 권한 오류 등으로 실패하면 그대로 멈추지 말고 자동으로 PR fallback 경로로 전환한다.\n\
     - PR fallback: `origin/main` 위에 clean branch를 다시 만들고 cherry-pick 후 push, PR open, auto-merge enable 까지 진행한다.\n\
     - direct push 와 PR fallback 이 모두 실패하면 대기 상태로 두지 말고 실패 사유를 명시적으로 남긴다.";
const PR_ALWAYS_COMPLETION_CONTRACT: &str = "- `merge_strategy_mode=pr-always` 계약:\n\
     - 처음부터 별도 브랜치에서 작업하고 `main` 에 직접 커밋하지 않는다.\n\
     - 구현/검증 후 브랜치를 push 하고 PR 을 연다.\n\
     - PR 생성 후 review 요청과 auto-merge enable 까지 수행한다.\n\
     - 이 모드의 완료 조건은 direct push 가 아니라 `PR open + auto-merge enabled` 이다.";

#[derive(Debug, Clone, Default)]
pub(crate) struct CurrentTaskContext<'a> {
    pub(crate) dispatch_id: Option<&'a str>,
    pub(crate) card_id: Option<&'a str>,
    pub(crate) dispatch_title: Option<&'a str>,
    pub(crate) dispatch_context: Option<&'a str>,
    pub(crate) card_title: Option<&'a str>,
    pub(crate) github_issue_url: Option<&'a str>,
}

fn context_compression_guidance() -> String {
    format!(
        "[Context Compression]\n\
         When conversation compaction happens (`/compact`, automatic compaction, or equivalent summarization), \
         rewrite prior context using these sections in order: {CONTEXT_COMPRESSION_SECTION_ORDER}.\n\
         - Keep each section short, factual, and focused on the latest state.\n\
         - Preserve unresolved blockers, assumptions, failures, and the latest user intent.\n\
         - In `Files`, list only files that still matter and why they matter.\n\
         - Replace stale tool chatter, raw logs, and old command output with placeholders like {STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE}.\n\
         - Prefer outcomes and follow-up implications over verbatim output, and drop already-resolved repetition once summarized."
    )
}

fn tool_output_efficiency_guidance() -> &'static str {
    "[Tool Output Efficiency]\n\
     Large tool results persist in context and increase cost for every subsequent turn.\n\
     - Bash/Read: If output would exceed 10 lines, summarize the result instead of pasting raw output\n\
     - Bash: Use LIMIT clauses for SQL, pipe to head/grep for filtering, avoid tail with large line counts\n\
     - Read: Use offset/limit to read specific sections; do not read entire files when a section is enough\n\
     - Grep: Set head_limit, use narrow glob/type filters, avoid broad patterns that match hundreds of lines\n\
     - Prefer targeted queries over exhaustive dumps"
}

fn parse_dispatch_context(dispatch_context: Option<&str>) -> Option<serde_json::Value> {
    dispatch_context.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
}

fn json_string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|items| items.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn render_string_list(label: &str, items: &[String], limit: usize) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut lines = items
        .iter()
        .take(limit)
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>();
    if items.len() > limit {
        lines.push(format!("- ... {} more", items.len() - limit));
    }
    Some(format!("{label}:\n{}", lines.join("\n")))
}

fn render_dispatch_context_section(
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> Option<String> {
    let context = parse_dispatch_context(dispatch_context)?;
    let mut sections = Vec::new();

    if let Some(value) = context.get("resumed_from").and_then(|value| value.as_str()) {
        sections.push(format!("Dispatch Trigger: resume from {value}"));
    } else if context
        .get("retry")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        sections.push("Dispatch Trigger: retry".to_string());
    } else if context
        .get("redispatch")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        sections.push("Dispatch Trigger: redispatch".to_string());
    } else if context
        .get("auto_queue")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        sections.push("Dispatch Trigger: auto-queue".to_string());
    }

    let reset_provider_state = context
        .get("reset_provider_state")
        .and_then(|value| value.as_bool())
        .or_else(|| {
            context
                .get("force_new_session")
                .and_then(|value| value.as_bool())
        })
        .unwrap_or(false);
    let recreate_tmux = context
        .get("recreate_tmux")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if reset_provider_state || recreate_tmux {
        let strategy = match (reset_provider_state, recreate_tmux) {
            (true, true) => {
                "Session Strategy: hard reset provider state and recreate tmux before working"
            }
            (true, false) => "Session Strategy: reset provider/model state before working",
            (false, true) => "Session Strategy: recreate tmux before working",
            (false, false) => unreachable!(),
        };
        sections.push(strategy.to_string());
    }

    let review_branch = context
        .get("branch")
        .and_then(|value| value.as_str())
        .or_else(|| {
            context
                .get("worktree_branch")
                .and_then(|value| value.as_str())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let review_repo = context
        .get("repo")
        .or_else(|| context.get("target_repo"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let review_issue = context.get("issue_number").and_then(|value| value.as_i64());
    let review_pr = context.get("pr_number").and_then(|value| value.as_i64());
    let reviewed_commit = context
        .get("reviewed_commit")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let merge_base = context
        .get("merge_base")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let verdict_endpoint = context
        .get("verdict_endpoint")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let decision_endpoint = context
        .get("decision_endpoint")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if dispatch_type == Some("review")
        || dispatch_type == Some("review-decision")
        || review_repo.is_some()
        || review_issue.is_some()
        || review_pr.is_some()
        || review_branch.is_some()
        || reviewed_commit.is_some()
        || context.get("review_mode").is_some()
        || verdict_endpoint.is_some()
        || decision_endpoint.is_some()
    {
        if let Some(repo) = review_repo {
            sections.push(format!("Review Repo: {repo}"));
        }
        if let Some(issue_number) = review_issue {
            sections.push(format!("Review Issue: #{issue_number}"));
        }
        if let Some(pr_number) = review_pr {
            sections.push(format!("Review PR: #{pr_number}"));
        }
        if let Some(review_mode) = context.get("review_mode").and_then(|value| value.as_str()) {
            sections.push(format!("Review Mode: {review_mode}"));
        }
        if let Some(branch) = review_branch {
            sections.push(format!("Review Branch: {branch}"));
        }
        if let Some(commit) = reviewed_commit {
            sections.push(format!("Reviewed Commit: {commit}"));
        }
        if let Some(base) = merge_base {
            sections.push(format!("Merge Base: {base}"));
        }
        if let Some(warning) = context
            .get("review_target_warning")
            .and_then(|value| value.as_str())
        {
            sections.push(format!("Review Target Warning: {warning}"));
        }
        if let Some(noop_reason) = context
            .get("noop_reason")
            .and_then(|value| value.as_str())
            .or_else(|| {
                context
                    .get("noop_result")
                    .and_then(|value| value.get("notes"))
                    .and_then(|value| value.as_str())
            })
        {
            sections.push(format!("Noop Reason:\n{noop_reason}"));
        }
        if let Some(scope_reminder) = context
            .get("review_quality_scope_reminder")
            .and_then(|value| value.as_str())
        {
            sections.push(format!("Review Scope Reminder: {scope_reminder}"));
        }
        let quality_checklist = json_string_list(context.get("review_quality_checklist"));
        if let Some(rendered) =
            render_string_list("Review Quality Checklist", &quality_checklist, 8)
        {
            sections.push(rendered);
        }
        if let Some(guidance) = context
            .get("review_verdict_guidance")
            .and_then(|value| value.as_str())
        {
            sections.push(format!("Review Verdict Guidance: {guidance}"));
        }
        if let Some(endpoint) = verdict_endpoint {
            sections.push(format!("Verdict Endpoint: {endpoint}"));
        }
        if let Some(endpoint) = decision_endpoint {
            sections.push(format!("Decision Endpoint: {endpoint}"));
        }
    }

    if let Some(verdict) = context.get("verdict").and_then(|value| value.as_str()) {
        sections.push(format!("Review Verdict: {verdict}"));
    }

    if let Some(phase_gate) = context
        .get("phase_gate")
        .and_then(|value| value.as_object())
    {
        if let Some(run_id) = phase_gate.get("run_id").and_then(|value| value.as_str()) {
            sections.push(format!("Phase Gate Run: {run_id}"));
        }
        if let Some(batch_phase) = phase_gate
            .get("batch_phase")
            .and_then(|value| value.as_i64())
        {
            sections.push(format!("Phase Gate Batch Phase: {batch_phase}"));
        }
        if let Some(next_phase) = phase_gate
            .get("next_phase")
            .and_then(|value| value.as_i64())
        {
            sections.push(format!("Phase Gate Next Phase: {next_phase}"));
        }
        if phase_gate
            .get("final_phase")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            sections.push("Phase Gate Final Phase: true".to_string());
        }
        if let Some(pass_verdict) = phase_gate
            .get("pass_verdict")
            .and_then(|value| value.as_str())
        {
            sections.push(format!("Phase Gate Pass Verdict: {pass_verdict}"));
        }
        let checks = json_string_list(phase_gate.get("checks"));
        if let Some(rendered) = render_string_list("Phase Gate Checks", &checks, 8) {
            sections.push(rendered);
        }
        let work_items = json_string_list(phase_gate.get("work_items"));
        if let Some(rendered) = render_string_list("Phase Gate Work Items", &work_items, 8) {
            sections.push(rendered);
        }
        let issues = phase_gate
            .get("issue_numbers")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_i64())
            .map(|issue| format!("#{issue}"))
            .collect::<Vec<_>>();
        if !issues.is_empty() {
            sections.push(format!("Phase Gate Issues: {}", issues.join(", ")));
        }
    }

    if let Some(ci_recovery) = context
        .get("ci_recovery")
        .and_then(|value| value.as_object())
    {
        if let Some(job_name) = ci_recovery.get("job_name").and_then(|value| value.as_str()) {
            sections.push(format!("CI Recovery Job: {job_name}"));
        }
        if let Some(reason) = ci_recovery.get("reason").and_then(|value| value.as_str()) {
            sections.push(format!("CI Failure Reason: {reason}"));
        }
        if let Some(run_url) = ci_recovery.get("run_url").and_then(|value| value.as_str()) {
            sections.push(format!("CI Run URL: {run_url}"));
        }
        if let Some(log_excerpt) = ci_recovery
            .get("log_excerpt")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sections.push(format!("CI Log Excerpt:\n{log_excerpt}"));
        }
    }

    (!sections.is_empty()).then(|| format!("Dispatch Context:\n{}", sections.join("\n\n")))
}

fn render_dispatch_contract(
    dispatch_type: Option<&str>,
    current_task: &CurrentTaskContext<'_>,
) -> Option<String> {
    match dispatch_type {
        Some("implementation") | Some("rework") => {
            let merge_strategy_mode = parse_dispatch_context(current_task.dispatch_context)
                .and_then(|context| {
                    context
                        .get("merge_strategy_mode")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "direct-first".to_string());
            let mode_contract = if merge_strategy_mode == "pr-always" {
                PR_ALWAYS_COMPLETION_CONTRACT
            } else {
                DIRECT_FIRST_COMPLETION_CONTRACT
            };
            let patch_guidance = current_task.dispatch_id.map(|dispatch_id| {
                format!(
                    "- 완료 시 `PATCH /api/dispatches/{dispatch_id}` result 에 `completed_commit`(최종 HEAD SHA)을 반드시 포함한다.\n\
                     - 예시 body: `{{\"status\":\"completed\",\"result\":{{\"summary\":\"결과 요약\",\"completed_commit\":\"<HEAD SHA>\"}}}}`"
                )
            });
            Some(format!(
                "[Dispatch Contract]\n{DISPATCH_CONTRACT_COMMON}\n{mode_contract}{}",
                patch_guidance
                    .map(|guidance| format!("\n{guidance}"))
                    .unwrap_or_default()
            ))
        }
        Some("review") => {
            let dispatch_id = current_task.dispatch_id?;
            Some(format!(
                "[Dispatch Contract]\n\
                 - 응답 첫 줄에 반드시 `VERDICT: pass|improve|reject|rework` 중 하나를 적는다.\n\
                 - 리뷰 결과는 GitHub issue 코멘트로 남긴다.\n\
                 - verdict 제출 경로: `POST /api/review-verdict` (`dispatch_id={dispatch_id}`).\n\
                 - `improve`/`reject`/`rework`면 구체적 `notes`와 `items`를 포함한다."
            ))
        }
        Some("review-decision") => {
            let card_id = current_task.card_id?;
            Some(format!(
                "[Dispatch Contract]\n\
                 - 카운터 리뷰 피드백을 읽고 `accept|dispute|dismiss` 중 하나를 고른다.\n\
                 - decision 제출 경로: `POST /api/review-decision` (`card_id={card_id}`).\n\
                 - accept는 피드백 수용 후 rework, dispute는 반박 후 재리뷰, dismiss는 무시 후 done 경로다."
            ))
        }
        Some("e2e-test") | Some("consultation") | Some("phase-gate") | Some("pm-decision") => {
            let dispatch_id = current_task.dispatch_id?;
            Some(format!(
                "[Dispatch Contract]\n\
                 - 완료 시 `PATCH /api/dispatches/{dispatch_id}`로 dispatch를 종료한다.\n\
                 - 예시 body: `{{\"status\":\"completed\",\"result\":{{\"summary\":\"결과 요약\"}}}}`\n\
                 - review verdict API는 사용하지 않는다."
            ))
        }
        _ => Some(
            current_task.dispatch_id.map_or_else(
                || {
                    "[Dispatch Contract]\n\
                     - 작업 완료 후 해당 dispatch의 종료 경로를 확인하고 상태를 마무리한다.\n\
                     - review verdict/review-decision 전용 dispatch가 아니라면 일반 dispatch 종료 경로를 사용한다."
                        .to_string()
                },
                |dispatch_id| {
                    format!(
                        "[Dispatch Contract]\n\
                         - 완료 시 `PATCH /api/dispatches/{dispatch_id}`로 dispatch를 종료한다.\n\
                         - 예시 body: `{{\"status\":\"completed\",\"result\":{{\"summary\":\"결과 요약\"}}}}`\n\
                         - 별도 review verdict/review-decision 규칙이 없으면 이 경로를 기본으로 사용한다."
                    )
                },
            ),
        ),
    }
}

fn render_current_task_section(
    current_task: &CurrentTaskContext<'_>,
    dispatch_type: Option<&str>,
) -> Option<String> {
    let mut sections = Vec::new();

    if let Some(dispatch_id) = current_task
        .dispatch_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("Dispatch ID: {dispatch_id}"));
    }
    if let Some(card_id) = current_task
        .card_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("Card ID: {card_id}"));
    }

    let card_title = current_task
        .card_title
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let dispatch_title = current_task
        .dispatch_title
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(title) = current_task
        .card_title
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        sections.push(format!("Title: {title}"));
    }
    if let Some(dispatch_title) = dispatch_title.filter(|title| Some(*title) != card_title) {
        sections.push(format!("Dispatch Brief:\n{dispatch_title}"));
    }
    if let Some(url) = current_task
        .github_issue_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        sections.push(format!("GitHub URL: {url}"));
    }

    if let Some(dispatch_context_section) =
        render_dispatch_context_section(dispatch_type, current_task.dispatch_context)
    {
        sections.push(dispatch_context_section);
    }

    if let Some(dispatch_contract) = render_dispatch_contract(dispatch_type, current_task) {
        sections.push(dispatch_contract);
    }

    (!sections.is_empty()).then(|| format!("[Current Task]\n{}", sections.join("\n\n")))
}

fn proactive_memory_guidance(
    memory_settings: Option<&ResolvedMemorySettings>,
    current_path: &str,
    channel_id: ChannelId,
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
    memento_mcp_available: bool,
) -> Option<String> {
    if profile != DispatchProfile::Full {
        return None;
    }

    let settings = memory_settings?;
    let (backend_name, read_tool, write_tool, extra_note) = match settings.backend {
        MemoryBackendKind::File => (
            "local",
            "`memory-read` skill",
            "`memory-write` skill",
            String::new(),
        ),
        MemoryBackendKind::Memento if !memento_mcp_available => return None,
        MemoryBackendKind::Memento => {
            let role_id = role_binding
                .map(|binding| binding.role_id.as_str())
                .unwrap_or(UNBOUND_MEMORY_ROLE_ID);
            let workspace_scope = current_path
                .trim()
                .split('/')
                .rev()
                .find(|segment| !segment.trim().is_empty())
                .map(sanitize_memento_workspace_segment)
                .unwrap_or_else(|| "default".to_string());
            let agent_workspace = resolve_memento_workspace(role_id, channel_id.get(), None);
            let agent_id = resolve_memento_agent_id(role_id, channel_id.get());
            (
                "memento",
                "`recall` MCP tool",
                "`remember` MCP tool",
                format!(
                    "\n- 스코프 규칙: 전역 정보는 `workspace`를 생략하고 `agentId`를 `default`로 둔다.\n\
                     - 스코프 규칙: 현재 프로젝트/도메인 사실과 기술 결정은 `workspace={workspace_scope}` + `agentId=default`로 저장한다.\n\
                     - 스코프 규칙: 이 에이전트만의 반복 에러, 작업 습관, 도구 사용 패턴은 `workspace={agent_workspace}` + `agentId={agent_id}`로 저장한다.\n\
                     - 현재 채널 힌트: workspace 스코프 이름은 `{workspace_scope}`, 에이전트 스코프 이름은 `{agent_workspace}`, 에이전트 ID는 `{agent_id}`다.\n\
                     - 원칙: 전역이 아니면 `workspace`를 명시하고, 에이전트 전용이 아니면 `agentId`는 `default`를 유지한다.\n\
                     - 참고: 턴 시작 `context` 주입과 세션 종료 시 `reflect`는 서버가 담당한다. 턴 중 보강만 `recall`/`remember`로 수행한다."
                ),
            )
        }
    };

    Some(format!(
        "\n\n[Proactive Memory Guidance]\n\
         이 세션에서 `{backend_name}` 메모리를 사용할 수 있습니다.\n\
         - 읽기: {read_tool} — 새로운 맥락 발견 시 추가 조회\n\
         - 쓰기: {write_tool} — 중요한 결정/에러/절차 발견 시 기록\n\
         - 트리거: 에러 원인 확정, 아키텍처 결정, 설정 변경, \"이전에\" 언급 시{extra_note}"
    ))
}

fn api_friction_guidance(profile: DispatchProfile) -> Option<String> {
    (profile == DispatchProfile::Full).then_some(
        "\n\n[ADK API Usage]\n\
         - ADK API 작업 전에는 먼저 `GET /api/docs` 또는 `GET /api/docs/{category}`로 관련 엔드포인트를 확인한다.\n\
         - API 호출이 실패하면 `sqlite3`나 legacy SQL 우회로로 돌아가지 말고 `/api/docs`에서 대안 엔드포인트를 다시 찾는다.\n\
         - 같은 엔드포인트 재시도, DB 직접 우회, 과도한 다단계 API 호출, `/api/docs` 없이 시행착오 탐색은 `API friction`으로 본다.\n\
         - API friction이 발생하면 응답 마지막 줄에 단일 행 JSON marker를 남긴다: `API_FRICTION: {\"endpoint\":\"/api/docs/kanban\",\"friction_type\":\"docs-bypass\",\"summary\":\"...\",\"workaround\":\"sqlite3\",\"suggested_fix\":\"...\",\"docs_category\":\"kanban\",\"keywords\":[\"/api/docs/kanban\",\"sqlite3\"]}`\n\
         - 서버가 이 marker를 사용자 응답에서 제거하고 `topic=api-friction`, `type=error`로 구조화 저장한다."
            .to_string(),
    )
}
/// Dispatch prompt profile — controls which system prompt sections are injected.
/// `Full` includes everything (used for implementation dispatches and normal turns).
/// `ReviewLite` strips peer agents, long-term memory, and skills to reduce token cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchProfile {
    /// Full system prompt — all sections included (implementation, normal turns)
    Full,
    /// Minimal prompt for review/review-decision dispatches.
    /// Includes: base context, shared agent rules, role binding.
    /// Excludes: skills, peer agent directory, long-term memory.
    ReviewLite,
}

impl DispatchProfile {
    /// Derive profile from dispatch type string.
    pub fn from_dispatch_type(dispatch_type: Option<&str>) -> Self {
        match dispatch_type {
            Some("review") | Some("review-decision") => Self::ReviewLite,
            _ => Self::Full,
        }
    }
}

#[derive(Clone, Debug)]
struct AgentPerformancePromptCacheEntry {
    hour_bucket: i64,
    section: Option<String>,
}

static AGENT_PERFORMANCE_PROMPT_CACHE: OnceLock<
    Mutex<HashMap<String, AgentPerformancePromptCacheEntry>>,
> = OnceLock::new();

/// Hour-aligned cache bucket used by the self-feedback prompt block (#1103).
/// Returning the same bucket guarantees the same cached string is served for
/// the entire hour, which is what makes the system prompt prefix stable
/// (Anthropic prefix cache hits) until the next hourly rollup tick.
fn agent_performance_hour_bucket() -> i64 {
    chrono::Utc::now().timestamp() / 3600
}

/// Look up the cached self-feedback section if it is still valid for the
/// supplied hour bucket. Returns `Some(Some(string))` for a fresh hit with a
/// payload, `Some(None)` for a fresh hit that previously resolved to `None`,
/// or `None` when no entry is fresh (caller must repopulate).
fn lookup_cached_agent_performance_section(
    cache_key: &str,
    hour_bucket: i64,
) -> Option<Option<String>> {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = cache.lock().ok()?;
    let entry = guard.get(cache_key)?;
    if entry.hour_bucket == hour_bucket {
        Some(entry.section.clone())
    } else {
        None
    }
}

fn store_agent_performance_section(cache_key: String, hour_bucket: i64, section: Option<String>) {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            cache_key,
            AgentPerformancePromptCacheEntry {
                hour_bucket,
                section,
            },
        );
    }
}

#[cfg(test)]
fn reset_agent_performance_cache_for_tests() {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.clear();
    }
}

/// Resolve the self-feedback section for the supplied role binding using a
/// caller-provided loader. Extracted so tests can drive the cache without
/// touching the live database (#1103).
fn agent_performance_prompt_section_with_loader<L>(
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
    hour_bucket: i64,
    loader: L,
) -> Option<String>
where
    L: FnOnce(&str) -> Result<Option<String>, String>,
{
    let binding = role_binding?;
    // A/B toggle (#1103): the channel-level `self_feedback_enabled` flag (named
    // `quality_feedback_injection_enabled` on the resolved binding) gates the
    // entire injection. ReviewLite turns also skip — they already strip
    // optional context for cost.
    if profile != DispatchProfile::Full || !binding.quality_feedback_injection_enabled {
        return None;
    }

    let cache_key = binding.role_id.clone();
    if let Some(cached) = lookup_cached_agent_performance_section(&cache_key, hour_bucket) {
        return cached;
    }

    let section = match loader(&binding.role_id) {
        Ok(section) => section,
        Err(error) => {
            tracing::warn!(
                role_id = %binding.role_id,
                "[quality] failed to load agent performance prompt section: {error}"
            );
            return None;
        }
    };

    store_agent_performance_section(cache_key, hour_bucket, section.clone());
    section
}

fn agent_performance_prompt_section(
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
) -> Option<String> {
    agent_performance_prompt_section_with_loader(
        role_binding,
        profile,
        agent_performance_hour_bucket(),
        |role_id| super::internal_api::get_agent_quality_prompt_section(role_id),
    )
}

fn render_channel_participants(
    discord_context: &str,
    channel_participants: &[UserRecord],
) -> String {
    let is_dm_context = discord_context.trim() == "Discord context: DM";
    let mut lines = vec!["Channel participants:".to_string()];
    if channel_participants.is_empty() {
        lines.push("- none recorded yet".to_string());
        return lines.join("\n");
    }

    for (idx, user) in channel_participants.iter().enumerate() {
        let mut line = format!("- {}", user.label());
        if is_dm_context && channel_participants.len() == 1 && idx == 0 {
            line.push_str(" [DM requester]");
        }
        lines.push(line);
    }
    lines.join("\n")
}

pub(super) fn build_system_prompt(
    discord_context: &str,
    channel_participants: &[UserRecord],
    current_path: &str,
    channel_id: ChannelId,
    token: &str,
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
    profile: DispatchProfile,
    dispatch_type: Option<&str>,
    current_task: Option<&CurrentTaskContext<'_>>,
    shared_knowledge: Option<&str>,
    longterm_catalog: Option<&str>,
    memory_settings: Option<&ResolvedMemorySettings>,
    memento_mcp_available: bool,
) -> String {
    let mut system_prompt_owned = format!(
        "You are chatting with a user through Discord.\n\
         {}\n\
         {}\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         agentdesk discord-sendfile <filepath> --channel {} --key {}\n\n\
         This delivers the file directly to the user's Discord channel.\n\
         Do NOT tell the user to use /down — use the command above instead.\n\n\
         When referencing files in your text, include the specific path (e.g. \"mod.rs:2700\"). \
         The user sees only your text output, not the tool calls themselves.\n\n\
         Discord formatting rules:\n\
         - Use inline `code` for short references. Reserve code blocks for actual code snippets.\n\
         - Keep messages concise and scannable on mobile. Prefer short paragraphs and bullet points.\n\
         - Avoid decorative separators or long horizontal lines.\n\n\
         This Discord channel does not support interactive prompts. Do NOT call AskUserQuestion, EnterPlanMode, or ExitPlanMode. \
         Ask in plain text if you need clarification.\n\n\
         Message author prefix: Direct user messages are prefixed as `[User: NAME (ID: N)]`; use that marker to distinguish speakers in shared channels.\n\n\
         Reply context: When a user message includes a [Reply context] tag, the user is responding to the **replied-to message**, \
         not necessarily your most recent message. Prioritize the reply target; ask if ambiguous.",
        discord_context,
        render_channel_participants(discord_context, channel_participants),
        current_path,
        channel_id.get(),
        discord_token_hash(token),
    );
    system_prompt_owned.push_str("\n\n");
    system_prompt_owned.push_str(tool_output_efficiency_guidance());

    if profile == DispatchProfile::Full {
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&context_compression_guidance());
    }

    if let Some(binding) = role_binding {
        // ReviewLite: inject minimal review rules instead of full shared prompt.
        // review and review-decision have different contracts:
        //   review          → read code, post review comment, submit verdict via /api/review-verdict
        //   review-decision → read counter-review feedback, submit accept/dispute/dismiss via /api/review-decision
        if profile == DispatchProfile::ReviewLite {
            system_prompt_owned.push_str(&match dispatch_type {
                Some("review-decision") => "\n\n[Review Decision Rules]\n\
                     - 한국어로 소통한다\n\
                     - 카운터 리뷰 피드백을 읽고 accept/dispute/dismiss 중 결정한다\n\
                     - POST /api/review-decision {card_id, decision, comment}로 결정을 제출한다\n\
                     - decision: accept(피드백 수용→rework), dispute(반박→재리뷰), dismiss(무시→done)"
                        .to_string(),
                _ => "\n\n[Review Rules]\n\
                     - 한국어로 소통한다\n\
                     - 리뷰 결과는 GitHub issue 코멘트로 남긴다\n\
                     - 리뷰 verdict 제출 후 dispatch를 완료한다"
                        .to_string(),
            });

            // #119: Inject review tuning guidance only for review dispatches (not review-decision).
            // Injecting into review-decision would bias the labeler's accept/dispute/dismiss judgment,
            // contaminating the FP/TP dataset that the guidance itself is derived from.
            if dispatch_type != Some("review-decision") {
                if let Some(guidance) = load_review_tuning_guidance() {
                    system_prompt_owned
                        .push_str("\n\n[Review Tuning — 과거 리뷰 정확도 기반 가이던스]\n");
                    system_prompt_owned.push_str(&guidance);
                }
            }
        } else if let Some(shared_prompt) = load_shared_prompt_for_profile("full") {
            // Full profile: inject the `full` + `all` sections of the shared agent prompt.
            // Profile-gated blocks (`review-lite`, `headless`) are stripped at load time.
            system_prompt_owned.push_str("\n\n[Shared Agent Rules]\n");
            system_prompt_owned.push_str(&shared_prompt);
            tracing::warn!(
                "  [role-map] Injected shared prompt ({} chars) for channel {}",
                shared_prompt.len(),
                channel_id.get()
            );
        }

        match load_role_prompt(binding) {
            Some(role_prompt) => {
                system_prompt_owned.push_str(
                    "\n\n[Channel Role Binding]\n\
                     The following role definition is authoritative for this Discord channel.\n\
                     You MUST answer as this role, stay within its scope, and follow its response contract.\n\
                     Do NOT override it with a generic assistant persona or by inferring a different role from repository files,\n\
                     unless the user explicitly asks you to audit or compare role definitions.\n\n",
                );
                system_prompt_owned.push_str(&role_prompt);
                tracing::warn!(
                    "  [role-map] Applied role '{}' for channel {}",
                    binding.role_id,
                    channel_id.get()
                );
            }
            None => {
                tracing::warn!(
                    "  [role-map] Failed to load prompt file '{}' for role '{}' (channel {})",
                    binding.prompt_file,
                    binding.role_id,
                    channel_id.get()
                );
            }
        }

        // SAK before LTM: placed here for cache prefix stability — SAK and
        // everything above it rarely changes, maximising Anthropic prefix cache hits.
        if let Some(sak) = shared_knowledge {
            system_prompt_owned.push_str("\n\n");
            system_prompt_owned.push_str(sak);
        }

        // ReviewLite: skip long-term memory and peer agents to save tokens
        if profile == DispatchProfile::Full {
            if let Some(catalog) = longterm_catalog {
                system_prompt_owned.push_str(
                    "\n\n[Long-term Memory]\n\
                     Available memory files for this agent. Use the Read tool to load full content when needed:\n",
                );
                system_prompt_owned.push_str(&catalog);
            }

            if binding.peer_agents_enabled {
                if let Some(peer_guidance) = render_peer_agent_guidance(&binding.role_id) {
                    system_prompt_owned.push_str("\n\n");
                    system_prompt_owned.push_str(&peer_guidance);
                }
            }
        }
    } else if let Some(sak) = shared_knowledge {
        // No role binding — still inject SAK (no LTM/peer agents to worry about)
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(sak);
    }

    if let Some(memory_guidance) = proactive_memory_guidance(
        memory_settings,
        current_path,
        channel_id,
        role_binding,
        profile,
        memento_mcp_available,
    ) {
        system_prompt_owned.push_str(&memory_guidance);
    }
    if let Some(api_friction_guidance) = api_friction_guidance(profile) {
        system_prompt_owned.push_str(&api_friction_guidance);
    }
    if let Some(performance_section) = agent_performance_prompt_section(role_binding, profile) {
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&performance_section);
    }

    if queued_turn {
        system_prompt_owned.push_str(
            "\n\n[Queued Turn Rules]\n\
             This user message was queued while another turn was running.\n\
             Treat ONLY the latest queued user message in this turn as actionable.\n\
             Do NOT repeat, combine, or continue prior queued messages unless the latest user message explicitly asks for that.\n\
             If the latest user message asks for an exact literal output, return exactly that literal output and nothing else.",
        );
    }
    if let Some(current_task_section) =
        current_task.and_then(|task| render_current_task_section(task, dispatch_type))
    {
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&current_task_section);
    }

    if profile == DispatchProfile::ReviewLite {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📉 ReviewLite prompt: {} chars (channel {})",
            system_prompt_owned.len(),
            channel_id.get()
        );
    }

    system_prompt_owned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: call build_system_prompt with minimal/default arguments (Full profile),
    /// while requiring each test to choose its own memento availability.
    fn call_build(
        discord_context: &str,
        current_path: &str,
        channel_id: u64,
        token: &str,
        memento_mcp_available: bool,
    ) -> String {
        build_system_prompt(
            discord_context,
            &[],
            current_path,
            ChannelId::new(channel_id),
            token,
            None,  // role_binding
            false, // queued_turn
            DispatchProfile::Full,
            None, // dispatch_type
            None, // current_task
            None, // shared_knowledge
            None, // longterm_catalog
            None, // memory_settings
            memento_mcp_available,
        )
    }

    #[test]
    fn test_build_system_prompt_includes_discord_context() {
        let output = call_build(
            "Channel: #general (guild: TestServer)",
            "/tmp/work",
            123456789,
            "fake-token",
            false,
        );
        assert!(
            output.contains("Channel: #general (guild: TestServer)"),
            "System prompt should contain the discord_context string"
        );
    }

    #[test]
    fn test_build_system_prompt_lists_channel_participants_without_inline_context_user() {
        let participants = [UserRecord::new(UserId::new(77), "Alice")];
        let output = build_system_prompt(
            "Discord context: channel #general (ID: 42)",
            &participants,
            "/tmp/work",
            ChannelId::new(42),
            "fake-token",
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
            false,
        );

        assert!(output.contains("Channel participants:\n- Alice (ID: 77)"));
        assert!(output.contains("[User: NAME (ID: N)]"));
        let discord_context_line = output
            .lines()
            .find(|line| line.starts_with("Discord context:"))
            .expect("discord context line");
        assert!(!discord_context_line.contains("user: Alice"));
        assert!(!discord_context_line.contains("ID: 77"));
    }

    #[test]
    fn test_build_system_prompt_marks_dm_single_participant() {
        let participants = [UserRecord::new(UserId::new(77), "Alice")];
        let output = build_system_prompt(
            "Discord context: DM",
            &participants,
            "/tmp/work",
            ChannelId::new(42),
            "fake-token",
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
            false,
        );

        assert!(output.contains("Channel participants:\n- Alice (ID: 77) [DM requester]"));
    }

    #[test]
    fn test_build_system_prompt_includes_cwd() {
        let output = call_build("ctx", "/home/user/projects", 1, "tok", false);
        assert!(
            output.contains("Current working directory: /home/user/projects"),
            "System prompt should contain the current working directory"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_file_send_command() {
        let output = call_build("ctx", "/tmp", 1, "tok", false);
        assert!(
            output.contains("agentdesk discord-sendfile"),
            "System prompt should contain the agentdesk discord-sendfile command"
        );
    }

    #[test]
    fn test_build_system_prompt_disables_interactive_tools() {
        let output = call_build("ctx", "/tmp", 1, "tok", false);
        assert!(
            output.contains("does not support interactive prompts"),
            "System prompt should warn that interactive tools are disabled"
        );
        assert!(
            output.contains("Do NOT call AskUserQuestion"),
            "System prompt should instruct not to use interactive tools"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_context_compression_guidance() {
        let output = call_build("ctx", "/tmp", 1, "tok", false);
        assert!(output.contains("[Context Compression]"));
        assert!(output.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
        assert!(output.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
    }

    #[test]
    fn test_build_system_prompt_includes_tool_output_efficiency_guidance() {
        let output = call_build("ctx", "/tmp", 1, "tok", false);
        assert!(output.contains("[Tool Output Efficiency]"));
        assert!(output.contains("Large tool results persist in context"));
        assert!(output.contains("If output would exceed 10 lines"));
        assert!(output.contains("Use LIMIT clauses for SQL"));
        assert!(output.contains("Use offset/limit to read specific sections"));
        assert!(output.contains("do not read entire files"));
        assert!(output.contains("Set head_limit"));
    }

    #[test]
    fn test_build_system_prompt_includes_api_friction_guidance() {
        let output = call_build("ctx", "/tmp", 1, "tok", false);
        assert!(output.contains("[ADK API Usage]"));
        assert!(output.contains("GET /api/docs/{category}"));
        assert!(output.contains("API_FRICTION:"));
        assert!(output.contains("topic=api-friction"));
    }

    #[test]
    fn test_dispatch_profile_from_dispatch_type() {
        assert_eq!(
            DispatchProfile::from_dispatch_type(None),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("implementation")),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("review")),
            DispatchProfile::ReviewLite
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("review-decision")),
            DispatchProfile::ReviewLite
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("e2e-test")),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("consultation")),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("rework")),
            DispatchProfile::Full
        );
    }

    #[test]
    fn test_empty_skills_notice_omits_skills_for_full_profile() {
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
            false,
        );

        assert!(!prompt.contains("Available skills"));
        assert!(!prompt.contains("descriptions only"));
        assert!(!prompt.contains("`SKILL.md`"));
    }

    #[test]
    fn test_review_lite_omits_context_compression_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            None,
            false,
        );

        assert!(!prompt.contains("[Context Compression]"));
        assert!(!prompt.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
        assert!(!prompt.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
    }

    #[test]
    fn test_review_lite_includes_tool_output_efficiency_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            None,
            false,
        );

        assert!(prompt.contains("[Tool Output Efficiency]"));
        assert!(prompt.contains("Prefer targeted queries over exhaustive dumps"));
    }

    #[test]
    fn test_review_decision_gets_decision_rules() {
        use super::super::settings::RoleBinding;
        let binding = RoleBinding {
            role_id: "test-agent".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
        };
        let review_prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            None,
            false,
        );
        let decision_prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review-decision"),
            None,
            None,
            None,
            None,
            false,
        );
        // review should NOT contain decision API
        assert!(!review_prompt.contains("/api/review-decision"));
        assert!(review_prompt.contains("[Review Rules]"));
        // review-decision should contain decision API and options
        assert!(decision_prompt.contains("/api/review-decision"));
        assert!(decision_prompt.contains("accept/dispute/dismiss"));
        assert!(decision_prompt.contains("[Review Decision Rules]"));
    }

    #[test]
    fn test_full_prompt_omits_peer_agent_directory_when_disabled() {
        use super::super::settings::RoleBinding;

        let binding = RoleBinding {
            role_id: "spark".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
        };

        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1488022491992424448),
            "tok",
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
            false,
        );

        assert!(!prompt.contains("[Peer Agent Directory]"));
    }

    #[test]
    fn test_full_prompt_renders_supplied_longterm_catalog() {
        use super::super::settings::RoleBinding;

        let binding = RoleBinding {
            role_id: "spark".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
        };

        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            Some("- facts.md: deployment notes"),
            None,
            false,
        );

        assert!(prompt.contains("[Long-term Memory]"));
        assert!(prompt.contains("facts.md"));
    }

    #[test]
    fn test_full_prompt_injects_memento_memory_guidance() {
        use super::super::settings::RoleBinding;

        let binding = RoleBinding {
            role_id: "project-agentdesk".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
        };
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/Users/test/.adk/release/workspaces/agentdesk",
            ChannelId::new(1),
            "tok",
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            }),
            true,
        );

        assert!(prompt.contains("[Proactive Memory Guidance]"));
        assert!(prompt.contains("`recall` MCP tool"));
        assert!(prompt.contains("`remember` MCP tool"));
        assert!(prompt.contains("`context`"));
        assert!(prompt.contains("`reflect`"));
        assert!(prompt.contains("`workspace`를 생략"));
        assert!(prompt.contains("`workspace=agentdesk` + `agentId=default`"));
        assert!(
            prompt
                .contains("`workspace=agentdesk-project-agentdesk` + `agentId=project-agentdesk`")
        );
        assert!(prompt.contains("workspace 스코프 이름은 `agentdesk`"));
        assert!(!prompt.contains("tool_feedback("));
    }

    #[test]
    fn test_full_prompt_omits_memento_memory_guidance_without_mcp() {
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/Users/test/.adk/release/workspaces/agentdesk",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            }),
            false,
        );

        assert!(!prompt.contains("[Proactive Memory Guidance]"));
        assert!(!prompt.contains("`recall` MCP tool"));
        assert!(!prompt.contains("`remember` MCP tool"));
    }

    #[test]
    fn test_review_lite_omits_memory_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::File,
                ..ResolvedMemorySettings::default()
            }),
            false,
        );

        assert!(!prompt.contains("[Proactive Memory Guidance]"));
        assert!(!prompt.contains("`memory-read`"));
        assert!(!prompt.contains("`memory-write`"));
    }

    #[test]
    fn test_build_system_prompt_appends_current_task_after_queued_turn_rules() {
        let current_task = CurrentTaskContext {
            dispatch_id: Some("dispatch-570"),
            card_id: Some("card-570"),
            dispatch_title: Some("[Rework] fix: prompt context"),
            dispatch_context: None,
            card_title: Some("fix: prompt context"),
            github_issue_url: Some("https://github.com/itismyfield/AgentDesk/issues/570"),
        };
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            true,
            DispatchProfile::Full,
            Some("implementation"),
            Some(&current_task),
            None,
            None,
            None,
            false,
        );

        let queued_index = prompt.find("[Queued Turn Rules]").unwrap();
        let task_index = prompt.find("[Current Task]").unwrap();
        assert!(task_index > queued_index);
        assert!(prompt.contains("Dispatch ID: dispatch-570"));
        assert!(prompt.contains("Card ID: card-570"));
        assert!(prompt.contains("Dispatch Brief:\n[Rework] fix: prompt context"));
        assert!(prompt.contains("GitHub URL: https://github.com/itismyfield/AgentDesk/issues/570"));
        assert!(prompt.contains("Title: fix: prompt context"));
        assert!(prompt.contains("`OUTCOME: noop`"));
        assert!(!prompt.contains("Issue Body:"));
        assert!(!prompt.contains("DoD:"));
    }

    #[test]
    fn test_build_system_prompt_renders_dispatch_context_and_completion_contract() {
        let dispatch_context = serde_json::json!({
            "repo": "owner/repo",
            "issue_number": 671,
            "pr_number": 812,
            "review_mode": "noop_verification",
            "branch": "wt/671-dispatch",
            "reviewed_commit": "abc12345deadbeef",
            "merge_base": "1122334455667788",
            "noop_reason": "feature already exists",
            "review_quality_checklist": ["edge case", "error handling"],
            "review_verdict_guidance": "quality issue가 보이면 improve",
            "verdict_endpoint": "POST /api/review-verdict",
            "ci_recovery": {
                "job_name": "dashboard-build",
                "reason": "Code job failed: dashboard-build",
                "run_url": "https://github.com/example/actions/runs/1"
            }
        });
        let dispatch_context_raw = dispatch_context.to_string();
        let current_task = CurrentTaskContext {
            dispatch_id: Some("dispatch-review-671"),
            card_id: Some("card-671"),
            dispatch_title: Some("[Review R2] card-671"),
            dispatch_context: Some(&dispatch_context_raw),
            card_title: Some("fix: dispatch message"),
            github_issue_url: None,
        };
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            Some(&current_task),
            None,
            None,
            None,
            false,
        );

        assert!(prompt.contains("Review Repo: owner/repo"));
        assert!(prompt.contains("Review Issue: #671"));
        assert!(prompt.contains("Review PR: #812"));
        assert!(prompt.contains("Review Mode: noop_verification"));
        assert!(prompt.contains("Review Branch: wt/671-dispatch"));
        assert!(prompt.contains("Reviewed Commit: abc12345deadbeef"));
        assert!(prompt.contains("Verdict Endpoint: POST /api/review-verdict"));
        assert!(prompt.contains("CI Recovery Job: dashboard-build"));
        assert!(prompt.contains("`POST /api/review-verdict` (`dispatch_id=dispatch-review-671`)"));
        assert!(prompt.contains("Review Quality Checklist"));
    }

    #[test]
    fn test_review_decision_identifiers_render_in_current_task_but_not_rules_section() {
        use super::super::settings::RoleBinding;

        let dispatch_context = serde_json::json!({
            "repo": "owner/repo",
            "issue_number": 692,
            "pr_number": 366,
            "reviewed_commit": "feedfacecafebeef",
            "decision_endpoint": "POST /api/review-decision",
            "verdict": "rework"
        });
        let dispatch_context_raw = dispatch_context.to_string();
        let current_task = CurrentTaskContext {
            dispatch_id: Some("dispatch-decision-692"),
            card_id: Some("card-692"),
            dispatch_title: Some("[리뷰 검토] card-692"),
            dispatch_context: Some(&dispatch_context_raw),
            card_title: Some("refactor: self-contained review decision"),
            github_issue_url: Some("https://github.com/itismyfield/AgentDesk/issues/692"),
        };
        let binding = RoleBinding {
            role_id: "test-agent".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
        };

        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review-decision"),
            Some(&current_task),
            None,
            None,
            None,
            false,
        );

        let rules_start = prompt.find("[Review Decision Rules]").unwrap();
        let task_start = prompt.find("[Current Task]").unwrap();
        let rules_section = &prompt[rules_start..task_start];

        assert!(prompt.contains("Review Repo: owner/repo"));
        assert!(prompt.contains("Review Issue: #692"));
        assert!(prompt.contains("Review PR: #366"));
        assert!(prompt.contains("Reviewed Commit: feedfacecafebeef"));
        assert!(prompt.contains("Decision Endpoint: POST /api/review-decision"));
        assert!(rules_section.contains("POST /api/review-decision {card_id, decision, comment}"));
        assert!(!rules_section.contains("owner/repo"));
        assert!(!rules_section.contains("#366"));
        assert!(!rules_section.contains("feedfacecafebeef"));
    }

    #[test]
    fn test_build_system_prompt_keeps_dispatch_contract_when_context_is_otherwise_empty() {
        let current_task = CurrentTaskContext::default();
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::Full,
            Some("implementation"),
            Some(&current_task),
            None,
            None,
            None,
            false,
        );

        assert!(prompt.contains("[Current Task]"));
        assert!(prompt.contains("[Dispatch Contract]"));
        assert!(prompt.contains("`OUTCOME: noop`"));
        assert!(prompt.contains("`git push origin HEAD:main`"));
        assert!(prompt.contains("PR fallback"));
        assert!(!prompt.contains("Dispatch ID:"));
        assert!(!prompt.contains("GitHub URL:"));
    }

    #[test]
    fn test_build_system_prompt_uses_direct_first_completion_contract_by_default() {
        let dispatch_context = serde_json::json!({
            "merge_strategy_mode": "direct-first"
        });
        let dispatch_context_raw = dispatch_context.to_string();
        let current_task = CurrentTaskContext {
            dispatch_id: Some("dispatch-direct-1"),
            dispatch_context: Some(&dispatch_context_raw),
            ..CurrentTaskContext::default()
        };
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::Full,
            Some("implementation"),
            Some(&current_task),
            None,
            None,
            None,
            false,
        );

        assert!(prompt.contains("`merge_strategy_mode=direct-first`"));
        assert!(prompt.contains("`git push origin HEAD:main`"));
        assert!(prompt.contains("PR fallback"));
        assert!(prompt.contains("PATCH /api/dispatches/dispatch-direct-1"));
        assert!(prompt.contains("\"completed_commit\":\"<HEAD SHA>\""));
        assert!(
            prompt.contains("`▶ Ready for input (type message + Enter)` 는 완료 마커가 아니다.")
        );
    }

    #[test]
    fn test_build_system_prompt_uses_pr_always_completion_contract_when_requested() {
        let dispatch_context = serde_json::json!({
            "merge_strategy_mode": "pr-always"
        });
        let dispatch_context_raw = dispatch_context.to_string();
        let current_task = CurrentTaskContext {
            dispatch_context: Some(&dispatch_context_raw),
            ..CurrentTaskContext::default()
        };
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::Full,
            Some("implementation"),
            Some(&current_task),
            None,
            None,
            None,
            false,
        );

        assert!(prompt.contains("`merge_strategy_mode=pr-always`"));
        assert!(prompt.contains("별도 브랜치에서 작업"));
        assert!(prompt.contains("PR 을 연다"));
        assert!(prompt.contains("auto-merge enable"));
        assert!(
            prompt.contains("`▶ Ready for input (type message + Enter)` 는 완료 마커가 아니다.")
        );
    }

    #[test]
    fn test_build_system_prompt_uses_default_dispatch_contract_for_unknown_dispatch_type() {
        let current_task = CurrentTaskContext {
            dispatch_id: Some("dispatch-generic-1"),
            ..CurrentTaskContext::default()
        };
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp",
            ChannelId::new(1),
            "tok",
            None,
            false,
            DispatchProfile::Full,
            None,
            Some(&current_task),
            None,
            None,
            None,
            false,
        );

        assert!(prompt.contains("[Dispatch Contract]"));
        assert!(prompt.contains("PATCH /api/dispatches/dispatch-generic-1"));
        assert!(prompt.contains("별도 review verdict/review-decision 규칙이 없으면"));
    }

    // NOTE: The _shared.prompt.md content assertion test was removed when
    // per-agent prompts moved out of the repo (operator-private content, now
    // canonical in the operator's Obsidian vault — see docs/source-of-truth.md).
    // Content-level validation now lives with the prompt author's editor workflow.

    // ─────────────────────────────────────────────────────────────────────
    // #1103 — Self-feedback prompt block tests
    //
    // These tests cover the *cache* and *channel A/B toggle* layers. The
    // formatter and rework category classifier are tested directly in
    // `internal_api::self_feedback_tests` against `AgentQualitySnapshot` so
    // they don't need a Postgres pool.
    // ─────────────────────────────────────────────────────────────────────

    use super::super::settings::RoleBinding;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serialise the cache-aware tests — they share a process-wide static
    /// cache, so cargo's parallel test runner would otherwise interleave
    /// `reset_agent_performance_cache_for_tests` calls with concurrent
    /// `lookup_cached_agent_performance_section` reads from sibling tests.
    fn cache_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn make_role_binding(role_id: &str, quality_feedback_enabled: bool) -> RoleBinding {
        RoleBinding {
            role_id: role_id.to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: quality_feedback_enabled,
            memory: Default::default(),
        }
    }

    #[test]
    fn self_feedback_section_is_cached_within_same_hour_bucket() {
        let _guard = cache_test_lock();
        reset_agent_performance_cache_for_tests();
        let binding = make_role_binding("role-cache-stable", true);
        let calls = AtomicUsize::new(0);
        let bucket = 42_i64;

        let first = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::Full,
            bucket,
            |role_id| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(format!(
                    "[Agent Performance — Last 7 Days]\nrole={role_id}"
                )))
            },
        );
        let second = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::Full,
            bucket,
            |_| {
                panic!("loader must not run for a same-bucket cache hit");
            },
        );

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "loader hit exactly once");
        assert!(first.unwrap().contains("role=role-cache-stable"));
    }

    #[test]
    fn self_feedback_section_recomputes_after_bucket_rollover() {
        let _guard = cache_test_lock();
        reset_agent_performance_cache_for_tests();
        let binding = make_role_binding("role-bucket-roll", true);

        let prev = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::Full,
            100,
            |_| Ok(Some("v1".to_string())),
        );
        let next = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::Full,
            101,
            |_| Ok(Some("v2".to_string())),
        );

        assert_eq!(prev, Some("v1".to_string()));
        assert_eq!(next, Some("v2".to_string()));
    }

    #[test]
    fn self_feedback_section_skipped_when_channel_toggle_off() {
        let _guard = cache_test_lock();
        reset_agent_performance_cache_for_tests();
        let binding = make_role_binding("role-toggle-off", false);
        let calls = AtomicUsize::new(0);

        let result = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::Full,
            7,
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some("should-not-render".to_string()))
            },
        );

        assert!(result.is_none());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "loader must not run when toggle is off"
        );
    }

    #[test]
    fn self_feedback_section_skipped_for_review_lite() {
        let _guard = cache_test_lock();
        reset_agent_performance_cache_for_tests();
        let binding = make_role_binding("role-review-lite", true);

        let result = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::ReviewLite,
            7,
            |_| Ok(Some("never".to_string())),
        );

        assert!(result.is_none());
    }

    #[test]
    fn self_feedback_section_caches_negative_lookup() {
        // Anthropic cache hit also relies on stability when the loader returns
        // None (e.g. fresh agent with no rollup row yet) — the cached `None`
        // must be served on subsequent calls so the prompt prefix stays
        // identical until the bucket rolls.
        let _guard = cache_test_lock();
        reset_agent_performance_cache_for_tests();
        let binding = make_role_binding("role-empty", true);
        let calls = AtomicUsize::new(0);

        let first = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::Full,
            9,
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
        );
        let second = agent_performance_prompt_section_with_loader(
            Some(&binding),
            DispatchProfile::Full,
            9,
            |_| panic!("loader must not run on cached negative hit"),
        );

        assert!(first.is_none());
        assert!(second.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn self_feedback_section_skips_when_role_binding_absent() {
        let _guard = cache_test_lock();
        reset_agent_performance_cache_for_tests();
        let result =
            agent_performance_prompt_section_with_loader(None, DispatchProfile::Full, 1, |_| {
                panic!("loader must not run without a binding")
            });
        assert!(result.is_none());
    }
}
