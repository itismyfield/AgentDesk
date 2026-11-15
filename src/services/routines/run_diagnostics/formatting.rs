//! Shared history formatting has no Discord delivery dependency.
use super::super::runtime::RoutineRunOutcome;
use super::super::store::{ClaimedRoutineRun, RoutineRecord};
use chrono::Datelike;
use serde_json::Value;

pub(in crate::services::routines) fn run_started_section(claimed: &ClaimedRoutineRun) -> String {
    routine_log_section(
        "루틴 실행 시작",
        vec![(
            "기본",
            vec![
                field_line("routine", &compact(&claimed.name, 80)),
                field_line("run", short_id(&claimed.run_id)),
                field_line("script", &script_ref_for_message(&claimed.script_ref, 120)),
            ],
        )],
    )
}

pub(in crate::services::routines) fn run_js_inputs_section(
    claimed: &ClaimedRoutineRun,
    observation_count: usize,
    automation_inventory_count: usize,
    checkpoint: Option<&Value>,
) -> String {
    let checkpoint_summary = checkpoint
        .map(checkpoint_state_summary)
        .unwrap_or_else(|| "없음".to_string());
    routine_log_section(
        "루틴 JS 처리 준비",
        vec![(
            "기본",
            vec![
                field_line("routine", &compact(&claimed.name, 80)),
                field_line("run", short_id(&claimed.run_id)),
                field_line("observations", observation_count.to_string()),
                field_line("inventory", automation_inventory_count.to_string()),
                field_line("checkpoint", checkpoint_summary),
            ],
        )],
    )
}

pub(in crate::services::routines) fn checkpoint_state_summary(cp: &Value) -> String {
    let mut state_counts: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    if let Some(candidates) = cp.get("candidates").and_then(Value::as_object) {
        for candidate in candidates.values() {
            let state = candidate
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            *state_counts.entry(state).or_insert(0) += 1;
        }
    }
    let mut parts: Vec<String> = state_counts
        .iter()
        .map(|(state, count)| format!("{state}:{count}"))
        .collect();
    if let Some(latest_rec) = cp
        .get("recommendations")
        .and_then(Value::as_array)
        .and_then(|recs| recs.last())
    {
        let pattern_id = latest_rec
            .get("pattern_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let summary = latest_rec
            .get("outcome_summary")
            .or_else(|| latest_rec.get("decision_summary"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !pattern_id.is_empty() {
            let rec_line = if summary.is_empty() {
                format!("최근 추천: {}", compact(pattern_id, 60))
            } else {
                format!(
                    "최근 추천: {} - {}",
                    compact(pattern_id, 60),
                    compact(summary, 120)
                )
            };
            parts.push(rec_line);
        }
    }
    if parts.is_empty() {
        "비어 있음".to_string()
    } else {
        parts.join(" / ")
    }
}

pub(in crate::services::routines) fn run_js_action_section(
    claimed: &ClaimedRoutineRun,
    action: &str,
    summary: Option<&str>,
    prompt: Option<&str>,
    checkpoint_update: bool,
) -> String {
    let mut sections = vec![(
        "기본",
        vec![
            field_line("routine", &compact(&claimed.name, 80)),
            field_line("run", short_id(&claimed.run_id)),
            field_line("action", format!("\"{}\"", action)),
            field_line("checkpoint", present_label(checkpoint_update)),
        ],
    )];
    if let Some(summary) = summary.filter(|value| !value.trim().is_empty()) {
        sections.push((
            "요약",
            vec![field_line("js", compact_multiline(summary, 600))],
        ));
    }
    if let Some(prompt) = prompt.filter(|value| !value.trim().is_empty()) {
        sections.push((
            "프롬프트",
            vec![field_line("agent", compact_multiline(prompt, 900))],
        ));
    }
    routine_log_section("루틴 JS 처리 결과", sections)
}

pub(in crate::services::routines) fn run_outcome_section(
    routine: &RoutineRecord,
    outcome: &RoutineRunOutcome,
    discord_log_warning: Option<&str>,
) -> String {
    let mut basic = vec![
        field_line("routine", &compact(&routine.name, 80)),
        field_line("run", short_id(&outcome.run_id)),
        field_line("action", format!("\"{}\"", outcome.action)),
        field_line("status", format!("\"{}\"", outcome.status)),
    ];
    if let Some(error) = outcome
        .error
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        basic.push(field_line("error", compact(error, 160)));
    }
    if let Some(agent_id) = outcome
        .result_json
        .as_ref()
        .and_then(|value| {
            value
                .get("agent_id")
                .or_else(|| value.get("failed_agent_id"))
        })
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        basic.push(field_line("agent", compact(agent_id, 80)));
    }
    if let Some(attempt_kind) = outcome
        .result_json
        .as_ref()
        .and_then(|value| value.get("attempt_kind"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        basic.push(field_line("attempt", compact(attempt_kind, 40)));
    }
    if let Some(retry_count) = outcome
        .result_json
        .as_ref()
        .and_then(|value| value.get("retry_count"))
        .and_then(Value::as_i64)
    {
        basic.push(field_line("retry_count", retry_count.to_string()));
    }
    if let Some(warning) = discord_log_warning.filter(|value| !value.trim().is_empty()) {
        basic.push(field_line("discord_log_warning", compact(warning, 160)));
    }
    let mut sections = vec![("기본", basic)];
    if let Some(summary) = outcome_summary_for_message(outcome) {
        sections.push(("요약", vec![field_line("result", compact(&summary, 220))]));
    }
    if let Some(response) = agent_response_preview(outcome.result_json.as_ref()) {
        sections.push((
            "에이전트 응답",
            vec![field_line("preview", compact_multiline(&response, 900))],
        ));
    }
    routine_log_section("루틴 실행 결과", sections)
}

pub(in crate::services::routines) fn routine_log_block_message(
    title: &str,
    sections: Vec<(&str, Vec<String>)>,
) -> String {
    let body = routine_log_sections_body(sections);
    format!(
        "[{}] {}\n```text\n{}\n```",
        routine_log_timestamp(),
        title,
        body
    )
}

pub(in crate::services::routines) fn routine_log_section(
    title: &str,
    sections: Vec<(&str, Vec<String>)>,
) -> String {
    format!(
        "[{}] {}\n{}",
        routine_log_timestamp(),
        title,
        routine_log_sections_body(sections)
    )
}

pub(in crate::services::routines) fn routine_log_sections_body(
    sections: Vec<(&str, Vec<String>)>,
) -> String {
    let mut lines = Vec::new();
    for (section, section_lines) in sections {
        if section_lines.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("[{}]", sanitize_code_block(section)));
        lines.extend(
            section_lines
                .into_iter()
                .map(|line| sanitize_code_block(&line)),
        );
    }
    lines.join("\n")
}

pub(in crate::services::routines) fn routine_run_progress_message(body: &str) -> String {
    format!(
        "[{}] 루틴 실행 로그\n```text\n{}\n```",
        routine_log_timestamp(),
        sanitize_code_block(body)
    )
}

pub(in crate::services::routines) fn field_line(label: &str, value: impl AsRef<str>) -> String {
    format!("{label}: {}", value.as_ref())
}

pub(in crate::services::routines) fn sanitize_code_block(value: &str) -> String {
    value.replace("```", "'''")
}

pub(in crate::services::routines) fn outcome_summary_for_message(
    outcome: &RoutineRunOutcome,
) -> Option<String> {
    let result = outcome.result_json.as_ref()?;
    for key in ["outcome_summary", "summary", "reason", "error"] {
        if let Some(value) = result
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

pub(in crate::services::routines) fn script_ref_for_message(
    value: &str,
    max_chars: usize,
) -> String {
    compact(&value.split('/').collect::<Vec<_>>().join(" / "), max_chars)
}

pub(in crate::services::routines) fn routine_log_timestamp() -> String {
    let now = chrono::Utc::now().with_timezone(&chrono_tz::Asia::Seoul);
    format!(
        "{} {} {}",
        now.format("%Y-%m-%d"),
        korean_weekday(now.weekday()),
        now.format("%H:%M:%S")
    )
}

pub(in crate::services::routines) fn korean_weekday(weekday: chrono::Weekday) -> &'static str {
    match weekday {
        chrono::Weekday::Mon => "월",
        chrono::Weekday::Tue => "화",
        chrono::Weekday::Wed => "수",
        chrono::Weekday::Thu => "목",
        chrono::Weekday::Fri => "금",
        chrono::Weekday::Sat => "토",
        chrono::Weekday::Sun => "일",
    }
}

pub(in crate::services::routines) fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

pub(in crate::services::routines) fn compact(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    let mut result: String = trimmed.chars().take(max_chars).collect();
    if trimmed.chars().count() > max_chars {
        result.push_str("...");
    }
    result
}

pub(in crate::services::routines) fn compact_multiline(value: &str, max_chars: usize) -> String {
    compact(&value.replace('\r', "").replace('\n', " / "), max_chars)
}

pub(in crate::services::routines) fn present_label(value: bool) -> &'static str {
    if value { "있음" } else { "없음" }
}

pub(in crate::services::routines) fn agent_response_preview(
    result_json: Option<&Value>,
) -> Option<String> {
    result_json?
        .get("assistant_message_preview")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
}
