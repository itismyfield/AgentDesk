//! Dispatch contract rendering — describes what the agent receives, splitting
//! ADK-provided contract text from user-derived dispatch context.

pub(super) const DISPATCH_CONTRACT_COMMON: &str = "- 구현이 불필요하고 현재 worktree에 tracked 변경이 전혀 없을 때만 응답 첫 줄에 `OUTCOME: noop`를 적고 근거를 설명한다.\n\
     - tracked 변경이 남아 있으면 noop를 사용하지 않는다.\n\
     - 관련 검증(`cargo build`, 좁은 테스트 등)을 직접 실행하고 결과를 최종 응답에 포함한다.\n\
     - `▶ Ready for input (type message + Enter)` 는 완료 마커가 아니다.";
pub(super) const DIRECT_FIRST_COMPLETION_CONTRACT: &str = "- `merge_strategy_mode=direct-first` 기본 계약:\n\
     - origin/main 기준으로 worktree에 타 작업이 섞이지 않았는지 확인한다.\n\
     - 커밋 메시지는 `#<issue> <title>` 형식을 사용한다.\n\
     - 완료 조건은 `git commit` 뒤 `git push origin HEAD:main` 성공이다.\n\
     - `git push` 가 non-fast-forward, pre-push hook, 권한 오류 등으로 실패하면 그대로 멈추지 말고 자동으로 PR fallback 경로로 전환한다.\n\
     - PR fallback: `origin/main` 위에 clean branch를 다시 만들고 cherry-pick 후 push, PR open, auto-merge enable 까지 진행한다.\n\
     - direct push 와 PR fallback 이 모두 실패하면 대기 상태로 두지 말고 실패 사유를 명시적으로 남긴다.";
pub(super) const PR_ALWAYS_COMPLETION_CONTRACT: &str = "- `merge_strategy_mode=pr-always` 계약:\n\
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

pub(super) fn parse_dispatch_context(dispatch_context: Option<&str>) -> Option<serde_json::Value> {
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

pub(super) fn render_dispatch_contract(
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
                 - verdict 제출 경로: `POST /api/reviews/verdict` (`dispatch_id={dispatch_id}`).\n\
                 - `improve`/`reject`/`rework`면 구체적 `notes`와 `items`를 포함한다."
            ))
        }
        Some("review-decision") => {
            let card_id = current_task.card_id?;
            Some(format!(
                "[Dispatch Contract]\n\
                 - 카운터 리뷰 피드백을 읽고 `accept|dispute|dismiss` 중 하나를 고른다.\n\
                 - decision 제출 경로: `POST /api/reviews/decision` (`card_id={card_id}`).\n\
                 - accept는 피드백 수용 후 rework, dispute는 반박 후 재리뷰, dismiss는 무시 후 done 경로다."
            ))
        }
        Some("phase-gate") => {
            let dispatch_id = current_task.dispatch_id?;
            let pass_verdict = parse_dispatch_context(current_task.dispatch_context)
                .and_then(|context| {
                    context
                        .get("phase_gate")
                        .and_then(|phase_gate| phase_gate.get("pass_verdict"))
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "phase_gate_passed".to_string());
            let example_body = serde_json::json!({
                "status": "completed",
                "result": {
                    "verdict": pass_verdict.clone(),
                    "summary": "결과 요약",
                    "checks": {
                        "merge_verified": { "status": "pass" },
                        "issue_closed": { "status": "pass" },
                        "build_passed": { "status": "pass" }
                    }
                }
            })
            .to_string();
            Some(format!(
                "[Dispatch Contract]\n\
                 - 완료 시 `PATCH /api/dispatches/{dispatch_id}`로 dispatch를 종료한다.\n\
                 - pass일 때 result.verdict는 반드시 `{pass_verdict}`로 넣는다.\n\
                 - result.checks에는 phase gate checks 각각의 pass/fail 상태를 넣는다.\n\
                 - 예시 body: `{example_body}`\n\
                 - review verdict API는 사용하지 않는다."
            ))
        }
        Some("e2e-test") | Some("consultation") | Some("pm-decision") => {
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

pub(super) fn render_current_task_section(
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
