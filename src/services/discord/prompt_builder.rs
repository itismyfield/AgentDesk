use super::settings::{
    MemoryBackendKind, ResolvedMemorySettings, discord_token_hash, load_review_tuning_guidance,
    load_role_prompt, load_shared_prompt, render_peer_agent_guidance,
};
use super::*;
use crate::github::dod::{DodItem, parse_dod_from_body, render_dod_markdown};
use crate::services::memory::{
    UNBOUND_MEMORY_ROLE_ID, resolve_memento_agent_id, resolve_memento_workspace,
    sanitize_memento_workspace_segment,
};

const CONTEXT_COMPRESSION_SECTION_ORDER: &str = "`Goal`, `Progress`, `Decisions`, `Files`, `Next`";
const STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE: &str =
    "[이전 결과 — 3줄 요약: cargo test failed in src/foo.rs because ...]";

#[derive(Debug, Clone, Default)]
pub(crate) struct CurrentTaskContext<'a> {
    pub(crate) card_title: Option<&'a str>,
    pub(crate) github_issue_url: Option<&'a str>,
    pub(crate) issue_body: Option<&'a str>,
    pub(crate) deferred_dod: Option<&'a serde_json::Value>,
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
     - Bash: Use LIMIT clauses for SQL, pipe to head/grep for filtering, avoid tail with large line counts\n\
     - Read: Use offset/limit to read specific sections, not entire large files\n\
     - Grep: Set head_limit, use narrow glob/type filters, avoid broad patterns that match hundreds of lines\n\
     - Prefer targeted queries over exhaustive dumps"
}

fn strip_dod_section(issue_body: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut in_dod_section = false;

    for line in issue_body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## ") {
            let header = trimmed[3..].trim().to_lowercase();
            if header == "dod" || header == "definition of done" {
                in_dod_section = true;
                continue;
            }
            if in_dod_section {
                in_dod_section = false;
            }
        }

        if in_dod_section {
            continue;
        }

        lines.push(line);
    }

    let stripped = lines.join("\n").trim().to_string();
    (!stripped.is_empty()).then_some(stripped)
}

fn deferred_dod_items(value: &serde_json::Value) -> Vec<DodItem> {
    let verified: std::collections::HashSet<String> = value
        .get("verified")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(str::to_string)
        .collect();

    value
        .get("items")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(|text| DodItem {
            text: text.to_string(),
            checked: verified.contains(text),
        })
        .collect()
}

fn render_current_task_section(current_task: &CurrentTaskContext<'_>) -> Option<String> {
    let mut sections = Vec::new();

    if let Some(title) = current_task
        .card_title
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        sections.push(format!("Title: {title}"));
    }
    if let Some(url) = current_task
        .github_issue_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        sections.push(format!("GitHub URL: {url}"));
    }
    if let Some(issue_body) = current_task.issue_body.and_then(strip_dod_section) {
        sections.push(format!("Issue Body:\n{issue_body}"));
    }

    let dod_items = current_task
        .deferred_dod
        .map(deferred_dod_items)
        .filter(|items| !items.is_empty())
        .or_else(|| {
            current_task
                .issue_body
                .map(parse_dod_from_body)
                .filter(|items| !items.is_empty())
        });

    if let Some(dod_items) = dod_items {
        sections.push(format!("DoD:\n{}", render_dod_markdown(&dod_items)));
    }

    (!sections.is_empty()).then(|| format!("[Current Task]\n{}", sections.join("\n\n")))
}

fn proactive_memory_guidance(
    memory_settings: Option<&ResolvedMemorySettings>,
    current_path: &str,
    channel_id: ChannelId,
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
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
        MemoryBackendKind::Mem0 => (
            "mem0",
            "`search_memory` MCP tool",
            "`add_memories` MCP tool",
            String::new(),
        ),
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
         - API 호출이 실패하면 `sqlite3`나 `agentdesk.db.query`로 우회하지 말고 `/api/docs`에서 대안 엔드포인트를 다시 찾는다.\n\
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

pub(super) fn build_system_prompt(
    discord_context: &str,
    current_path: &str,
    channel_id: ChannelId,
    token: &str,
    disabled_notice: &str,
    narrate_progress: bool,
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
    profile: DispatchProfile,
    dispatch_type: Option<&str>,
    current_task: Option<&CurrentTaskContext<'_>>,
    shared_knowledge: Option<&str>,
    longterm_catalog: Option<&str>,
    memory_settings: Option<&ResolvedMemorySettings>,
) -> String {
    let narration_guidance = if narrate_progress {
        "\n\nAlways keep the user informed about what you are doing. Briefly explain each step as you work \
         (e.g. \"Reading the file...\", \"Creating the script...\", \"Running tests...\")."
    } else {
        ""
    };
    let mut system_prompt_owned = format!(
        "You are chatting with a user through Discord.\n\
         {}\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         agentdesk discord-sendfile <filepath> --channel {} --key {}\n\n\
         This delivers the file directly to the user's Discord channel.\n\
         Do NOT tell the user to use /down — use the command above instead.{}\n\n\
         IMPORTANT: When reading, editing, or searching files, ALWAYS mention the specific file path and what you're looking for \
         (e.g. \"mod.rs:2700 부근의 시스템 프롬프트를 확인합니다\" not just \"코드를 확인합니다\"). \
         The user sees only your text output, not the tool calls themselves.\n\n\
         Discord formatting rules:\n\
         - Minimize code blocks. Use inline `code` for short references. Only use code blocks for actual code snippets the user needs.\n\
         - Keep messages concise and scannable on mobile screens. Prefer short paragraphs and bullet points.\n\
         - Avoid long horizontal lines or decorative separators.\n\n\
         IMPORTANT: The user is on Discord and CANNOT interact with any interactive prompts, dialogs, or confirmation requests. \
         All tools that require user interaction (such as AskUserQuestion, EnterPlanMode, ExitPlanMode) will NOT work. \
         Never use tools that expect user interaction. If you need clarification, just ask in plain text.\n\n\
         Reply context: When a user message includes a [Reply context] tag, the user is responding to the **replied-to message**, \
         not necessarily your most recent message. Prioritize the reply target over the latest message when interpreting user intent. \
         If ambiguous, ask which message the user is responding to. \
         Avoid mixing status reports and action questions in a single message — it makes the reply target unclear.{}",
        discord_context,
        current_path,
        channel_id.get(),
        discord_token_hash(token),
        narration_guidance,
        disabled_notice
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
        } else if let Some(shared_prompt) = load_shared_prompt() {
            // Full profile: inject complete shared agent prompt (AGENTS.md)
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
    ) {
        system_prompt_owned.push_str(&memory_guidance);
    }
    if let Some(api_friction_guidance) = api_friction_guidance(profile) {
        system_prompt_owned.push_str(&api_friction_guidance);
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
    if let Some(current_task_section) = current_task.and_then(render_current_task_section) {
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

    /// Helper: call build_system_prompt with minimal/default arguments (Full profile).
    fn call_build(
        discord_context: &str,
        current_path: &str,
        channel_id: u64,
        token: &str,
        disabled_notice: &str,
    ) -> String {
        build_system_prompt(
            discord_context,
            current_path,
            ChannelId::new(channel_id),
            token,
            disabled_notice,
            true,  // narrate_progress
            None,  // role_binding
            false, // queued_turn
            DispatchProfile::Full,
            None, // dispatch_type
            None, // current_task
            None, // shared_knowledge
            None, // longterm_catalog
            None, // memory_settings
        )
    }

    #[test]
    fn test_build_system_prompt_includes_discord_context() {
        let output = call_build(
            "Channel: #general (guild: TestServer)",
            "/tmp/work",
            123456789,
            "fake-token",
            "",
        );
        assert!(
            output.contains("Channel: #general (guild: TestServer)"),
            "System prompt should contain the discord_context string"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_cwd() {
        let output = call_build("ctx", "/home/user/projects", 1, "tok", "");
        assert!(
            output.contains("Current working directory: /home/user/projects"),
            "System prompt should contain the current working directory"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_file_send_command() {
        let output = call_build("ctx", "/tmp", 1, "tok", "");
        assert!(
            output.contains("agentdesk discord-sendfile"),
            "System prompt should contain the agentdesk discord-sendfile command"
        );
    }

    #[test]
    fn test_build_system_prompt_disables_interactive_tools() {
        let output = call_build("ctx", "/tmp", 1, "tok", "");
        assert!(
            output.contains("CANNOT interact with any interactive prompts"),
            "System prompt should warn that interactive tools are disabled"
        );
        assert!(
            output.contains("Never use tools that expect user interaction"),
            "System prompt should instruct not to use interactive tools"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_context_compression_guidance() {
        let output = call_build("ctx", "/tmp", 1, "tok", "");
        assert!(output.contains("[Context Compression]"));
        assert!(output.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
        assert!(output.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
    }

    #[test]
    fn test_build_system_prompt_includes_tool_output_efficiency_guidance() {
        let output = call_build("ctx", "/tmp", 1, "tok", "");
        assert!(output.contains("[Tool Output Efficiency]"));
        assert!(output.contains("Large tool results persist in context"));
        assert!(output.contains("Use LIMIT clauses for SQL"));
        assert!(output.contains("Use offset/limit to read specific sections"));
        assert!(output.contains("Set head_limit"));
    }

    #[test]
    fn test_build_system_prompt_includes_api_friction_guidance() {
        let output = call_build("ctx", "/tmp", 1, "tok", "");
        assert!(output.contains("[ADK API Usage]"));
        assert!(output.contains("GET /api/docs/{category}"));
        assert!(output.contains("API_FRICTION:"));
        assert!(output.contains("topic=api-friction"));
    }

    #[test]
    fn test_build_system_prompt_includes_narration_when_enabled() {
        let output = call_build("ctx", "/tmp", 1, "tok", "");
        assert!(output.contains("Always keep the user informed about what you are doing."));
        assert!(!output.contains("The user cannot see your tool calls"));
    }

    #[test]
    fn test_build_system_prompt_omits_narration_when_disabled() {
        let output = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            false,
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
        );

        assert!(!output.contains("Always keep the user informed about what you are doing."));
        assert!(!output.contains("The user cannot see your tool calls"));
        assert!(output.contains("ALWAYS mention the specific file path"));
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
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
        );

        assert!(!prompt.contains("Available skills"));
        assert!(!prompt.contains("descriptions only"));
        assert!(!prompt.contains("`SKILL.md`"));
    }

    #[test]
    fn test_review_lite_omits_context_compression_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            None,
        );

        assert!(!prompt.contains("[Context Compression]"));
        assert!(!prompt.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
        assert!(!prompt.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
    }

    #[test]
    fn test_review_lite_includes_tool_output_efficiency_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            None,
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
            memory: Default::default(),
        };
        let review_prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            None,
        );
        let decision_prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review-decision"),
            None,
            None,
            None,
            None,
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
            memory: Default::default(),
        };

        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1488022491992424448),
            "tok",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
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
            memory: Default::default(),
        };

        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            Some("- facts.md: deployment notes"),
            None,
        );

        assert!(prompt.contains("[Long-term Memory]"));
        assert!(prompt.contains("facts.md"));
    }

    #[test]
    fn test_full_prompt_injects_mem0_memory_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::Mem0,
                ..ResolvedMemorySettings::default()
            }),
        );

        assert!(prompt.contains("[Proactive Memory Guidance]"));
        assert!(prompt.contains("`search_memory` MCP tool"));
        assert!(prompt.contains("`add_memories` MCP tool"));
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
            memory: Default::default(),
        };
        let prompt = build_system_prompt(
            "ctx",
            "/Users/test/.adk/release/workspaces/agentdesk",
            ChannelId::new(1),
            "tok",
            "",
            true,
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
    fn test_review_lite_omits_memory_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::Mem0,
                ..ResolvedMemorySettings::default()
            }),
        );

        assert!(!prompt.contains("[Proactive Memory Guidance]"));
        assert!(!prompt.contains("`search_memory`"));
        assert!(!prompt.contains("`add_memories`"));
    }

    #[test]
    fn test_build_system_prompt_appends_current_task_after_queued_turn_rules() {
        let deferred_dod = serde_json::json!({
            "items": ["ship tests"],
            "verified": ["ship tests"]
        });
        let current_task = CurrentTaskContext {
            card_title: Some("fix: prompt context"),
            github_issue_url: Some("https://github.com/itismyfield/AgentDesk/issues/570"),
            issue_body: Some("## 배경\n\ncompact에서 사라짐\n\n## DoD\n- [ ] old item"),
            deferred_dod: Some(&deferred_dod),
        };
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            None,
            true,
            DispatchProfile::Full,
            Some("implementation"),
            Some(&current_task),
            None,
            None,
            None,
        );

        let queued_index = prompt.find("[Queued Turn Rules]").unwrap();
        let task_index = prompt.find("[Current Task]").unwrap();
        assert!(task_index > queued_index);
        assert!(prompt.contains("GitHub URL: https://github.com/itismyfield/AgentDesk/issues/570"));
        assert!(prompt.contains("Title: fix: prompt context"));
        assert!(prompt.contains("- [x] ship tests"));
        assert!(!prompt.contains("## DoD"));
    }

    #[test]
    fn test_build_system_prompt_omits_current_task_when_context_empty() {
        let current_task = CurrentTaskContext::default();
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            true,
            None,
            false,
            DispatchProfile::Full,
            Some("implementation"),
            Some(&current_task),
            None,
            None,
            None,
        );

        assert!(!prompt.contains("[Current Task]"));
    }

    #[test]
    fn test_shared_prompt_declares_discord_response_style_rules() {
        let shared_prompt = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("config/agents/_shared.prompt.md"),
        )
        .expect("shared prompt file should exist");

        assert!(shared_prompt.contains("## Discord Response Style"));
        assert!(shared_prompt.contains("`⏳ 대기 중...`"));
        assert!(shared_prompt.contains("raw 로그, JSON, 반복 출력은 그대로 덤프하지 않는다"));
    }
}
