use serde_json::Value;

const TARGET_TITLE_LIMIT: usize = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultHeaderMergeStatus {
    Noop,
    Pending,
    Merged,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResultHeader {
    status_line: String,
    target: String,
    next_action: String,
}

impl ResultHeader {
    fn new(
        status_line: impl Into<String>,
        target: Option<String>,
        next_action: impl Into<String>,
    ) -> Option<Self> {
        let status_line = status_line.into();
        let target = target?;
        let next_action = next_action.into();
        if status_line.trim().is_empty()
            || target.trim().is_empty()
            || next_action.trim().is_empty()
        {
            return None;
        }

        Some(Self {
            status_line,
            target,
            next_action,
        })
    }

    pub(crate) fn render(&self) -> String {
        format!(
            "{}\n대상: {}\n다음: {}",
            self.status_line, self.target, self.next_action
        )
    }
}

pub(crate) fn prepend_result_header(body: &str, header: Option<ResultHeader>) -> String {
    match header {
        Some(header) => format!("{}\n\n{}", header.render(), body),
        None => body.to_string(),
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let total = value.chars().count();
    if total <= limit {
        return value.to_string();
    }
    if limit <= 1 {
        return "…".chars().take(limit).collect();
    }

    let mut truncated: String = value.chars().take(limit - 1).collect();
    truncated.push('…');
    truncated
}

fn compact_title(title: &str) -> Option<String> {
    let first_line = title
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(title);
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    (!trimmed.is_empty()).then(|| truncate_chars(trimmed, TARGET_TITLE_LIMIT))
}

fn json_string_field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn context_i64_field(value: Option<&Value>, key: &str) -> Option<i64> {
    value
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_i64())
}

fn normalized_result_code(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-")
        .replace(' ', "-")
}

fn metadata_target(
    issue_number: Option<i64>,
    context_json: Option<&Value>,
    title: Option<&str>,
    card_id: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(repo) = json_string_field(context_json, "repo")
        .or_else(|| json_string_field(context_json, "target_repo"))
    {
        parts.push(format!("repo={repo}"));
    }
    if let Some(issue_number) = context_i64_field(context_json, "issue_number").or(issue_number) {
        parts.push(format!("issue=#{issue_number}"));
    }
    if let Some(pr_number) = context_i64_field(context_json, "pr_number") {
        parts.push(format!("pr=#{pr_number}"));
    }
    if let Some(commit) = json_string_field(context_json, "reviewed_commit") {
        parts.push(format!("commit={}", truncate_chars(commit, 12)));
    }
    if !parts.is_empty() {
        return Some(parts.join(", "));
    }

    title.and_then(compact_title).or_else(|| {
        card_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("card={}", truncate_chars(value, 48)))
    })
}

fn review_verdict_header_status(verdict: &str) -> Option<(&'static str, &'static str)> {
    match normalized_result_code(verdict).as_str() {
        "pass" | "approved" => Some(("리뷰 PASS · 진행 가능", "done으로 이동")),
        "improve" | "rework" => Some(("리뷰 REWORK · 수정 필요", "review-decision dispatch 진행")),
        "reject" | "rejected" => Some(("리뷰 REJECT · 수정 필요", "review-decision dispatch 진행")),
        "fail" | "failed" => Some(("리뷰 FAIL · 수정 필요", "review-decision dispatch 진행")),
        "blocked" => Some(("리뷰 BLOCKED · 확인 필요", "차단 원인 확인 후 재요청")),
        _ => None,
    }
}

fn build_review_verdict_result_header(
    verdict: &str,
    target: Option<String>,
    non_pass_next_action: Option<&'static str>,
) -> Option<ResultHeader> {
    let (status_line, next_action) = review_verdict_header_status(verdict)?;
    let next_action = if matches!(
        normalized_result_code(verdict).as_str(),
        "pass" | "approved"
    ) {
        next_action
    } else {
        non_pass_next_action.unwrap_or(next_action)
    };
    ResultHeader::new(status_line, target, next_action)
}

pub(crate) fn prepend_review_result_header(
    title: &str,
    issue_number: Option<i64>,
    review_context_json: Option<&Value>,
    verdict: &str,
    body: &str,
) -> String {
    let target = metadata_target(issue_number, review_context_json, Some(title), None);
    prepend_result_header(
        body,
        build_review_verdict_result_header(verdict, target, None),
    )
}

pub(crate) fn build_review_decision_dispatch_header(
    dispatch_type: Option<&str>,
    issue_number: Option<i64>,
    title: &str,
    context_json: &Value,
) -> Option<ResultHeader> {
    if dispatch_type != Some("review-decision") {
        return None;
    }

    let verdict = json_string_field(Some(context_json), "verdict")?;
    let target = metadata_target(issue_number, Some(context_json), Some(title), None);
    build_review_verdict_result_header(verdict, target, Some("accept/dispute/dismiss 결정"))
}

fn review_decision_header_status(
    result_json: Option<&Value>,
) -> Option<(&'static str, &'static str)> {
    let decision = normalized_result_code(json_string_field(result_json, "decision")?);
    let outcome = json_string_field(result_json, "outcome").map(normalized_result_code);

    match decision.as_str() {
        "accept" | "auto-accept" => Some(("리뷰 검토 ACCEPT · 수정 수용", "후속 상태 확인")),
        "dispute" if outcome.as_deref() == Some("scope-mismatch-closed") => {
            Some(("리뷰 검토 BLOCKED · 범위 불일치", "카드 종료 상태 확인"))
        }
        "dispute" => Some(("리뷰 검토 DISPUTE · 재리뷰 필요", "review dispatch 진행")),
        "dismiss" => Some(("리뷰 검토 DISMISS · 진행 가능", "done 상태 확인")),
        _ => None,
    }
}

pub(crate) fn build_review_decision_completion_header(
    result_json: Option<&Value>,
    context_json: Option<&Value>,
    card_id: &str,
) -> Option<ResultHeader> {
    let (status_line, next_action) = review_decision_header_status(result_json)?;
    let target = metadata_target(None, context_json, None, Some(card_id));
    ResultHeader::new(status_line, target, next_action)
}

fn explicit_work_result_code(result_json: Option<&Value>) -> Option<&str> {
    const KEYS: &[&str] = &[
        "result_status",
        "completion_status",
        "status",
        "outcome",
        "verdict",
    ];
    KEYS.iter()
        .find_map(|key| json_string_field(result_json, key))
}

fn explicit_work_header_status(
    result_json: Option<&Value>,
) -> Option<(&'static str, &'static str)> {
    match normalized_result_code(explicit_work_result_code(result_json)?).as_str() {
        "pass" | "passed" | "ok" | "success" | "succeeded" | "completed" | "done" => {
            Some(("작업 PASS · 진행 가능", "후속 단계 진행 가능"))
        }
        "fail" | "failed" | "failure" | "reject" | "rejected" => {
            Some(("작업 FAIL · 수정 필요", "실패 원인 반영 후 재실행"))
        }
        "improve" | "rework" | "please-revise" => {
            Some(("작업 REWORK · 수정 필요", "수정 후 재검증"))
        }
        "partial" | "partially-completed" => Some(("작업 PARTIAL · 확인 필요", "남은 항목 확인")),
        "blocked" | "defer" | "deferred" => Some(("작업 BLOCKED · 대기", "차단 원인 해소")),
        _ => None,
    }
}

fn work_completion_header_status(
    result_json: Option<&Value>,
    completed_without_changes: bool,
    merge_status: ResultHeaderMergeStatus,
) -> Option<(&'static str, &'static str)> {
    if let Some(explicit) = explicit_work_header_status(result_json) {
        return Some(explicit);
    }

    if completed_without_changes {
        return Some(("작업 PASS · 변경 없음", "후속 단계 진행 가능"));
    }

    match merge_status {
        ResultHeaderMergeStatus::Merged => Some(("작업 PASS · main 반영됨", "후속 단계 진행 가능")),
        ResultHeaderMergeStatus::Pending => {
            Some(("작업 PARTIAL · 머지 대기", "PR/merge 상태 확인"))
        }
        ResultHeaderMergeStatus::Noop => Some(("작업 PASS · 변경 없음", "후속 단계 진행 가능")),
        ResultHeaderMergeStatus::Unknown => None,
    }
}

pub(crate) fn build_work_completion_result_header(
    result_json: Option<&Value>,
    context_json: Option<&Value>,
    card_id: &str,
    completed_branch: Option<&str>,
    completed_without_changes: bool,
    merge_status: ResultHeaderMergeStatus,
) -> Option<ResultHeader> {
    let (status_line, next_action) =
        work_completion_header_status(result_json, completed_without_changes, merge_status)?;
    let target = metadata_target(None, context_json, None, None)
        .or_else(|| {
            completed_branch
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|branch| format!("branch={}", truncate_chars(branch, 64)))
        })
        .or_else(|| {
            let card_id = card_id.trim();
            (!card_id.is_empty()).then(|| format!("card={}", truncate_chars(card_id, 48)))
        });
    ResultHeader::new(status_line, target, next_action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn review_result_header_prepends_pass_from_metadata() {
        let context = json!({
            "issue_number": 3810,
            "pr_number": 123,
        });
        let body = "✅ [리뷰 통과] issue — done으로 이동";

        let message = prepend_review_result_header("issue", None, Some(&context), "pass", body);

        assert_eq!(
            message,
            "리뷰 PASS · 진행 가능\n\
             대상: issue=#3810, pr=#123\n\
             다음: done으로 이동\n\n\
             ✅ [리뷰 통과] issue — done으로 이동"
        );
    }

    #[test]
    fn review_result_header_omits_unknown_verdict() {
        let body = "verdict missing body";

        let message = prepend_review_result_header("issue", None, None, "unknown", body);

        assert_eq!(message, body);
    }

    #[test]
    fn review_decision_dispatch_header_renders_rework() {
        let context = json!({"verdict": "rework", "issue_number": 3810, "pr_number": 123});

        let header =
            build_review_decision_dispatch_header(Some("review-decision"), None, "title", &context)
                .expect("review-decision dispatch header");

        assert_eq!(
            header.render(),
            "리뷰 REWORK · 수정 필요\n\
             대상: issue=#3810, pr=#123\n\
             다음: accept/dispute/dismiss 결정"
        );
    }

    #[test]
    fn work_result_header_renders_partial_from_explicit_metadata() {
        let result = json!({"completion_status": "partial"});
        let context = json!({"issue_number": 3805});

        let header = build_work_completion_result_header(
            Some(&result),
            Some(&context),
            "card-1",
            Some("codex/issue-3805"),
            false,
            ResultHeaderMergeStatus::Pending,
        )
        .expect("partial header");

        assert_eq!(
            header.render(),
            "작업 PARTIAL · 확인 필요\n대상: issue=#3805\n다음: 남은 항목 확인"
        );
    }

    #[test]
    fn work_result_header_renders_blocked_only_from_explicit_metadata() {
        let result = json!({"outcome": "blocked"});
        let context = json!({"repo": "itismyfield/AgentDesk", "issue_number": 3810});

        let header = build_work_completion_result_header(
            Some(&result),
            Some(&context),
            "card-1",
            None,
            false,
            ResultHeaderMergeStatus::Unknown,
        )
        .expect("blocked header");

        assert_eq!(
            header.render(),
            "작업 BLOCKED · 대기\n\
             대상: repo=itismyfield/AgentDesk, issue=#3810\n\
             다음: 차단 원인 해소"
        );
    }

    #[test]
    fn work_result_header_omits_ambiguous_unknown_merge_status() {
        let header = build_work_completion_result_header(
            None,
            Some(&json!({"issue_number": 3810})),
            "card-1",
            None,
            false,
            ResultHeaderMergeStatus::Unknown,
        );

        assert!(header.is_none());
    }

    #[test]
    fn review_decision_completion_header_renders_structured_decision() {
        let result = json!({
            "decision": "dispute",
            "completion_source": "review_decision_api"
        });
        let context = json!({"issue_number": 3810});

        let header =
            build_review_decision_completion_header(Some(&result), Some(&context), "card-1")
                .expect("review-decision header");

        assert_eq!(
            header.render(),
            "리뷰 검토 DISPUTE · 재리뷰 필요\n대상: issue=#3810\n다음: review dispatch 진행"
        );
    }
}
