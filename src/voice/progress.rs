use std::time::Duration;

pub(crate) const PROGRESS_MARKER: &str = "VERBALCODING_PROGRESS:";
pub(crate) const PROGRESS_BATCH_MAX_CATEGORIES: usize = 3;
pub(crate) const PROGRESS_BATCH_MAX_EVENTS: usize = 5;
pub(crate) const PROGRESS_IDLE_NOTICE_INITIAL: Duration = Duration::from_secs(10);
pub(crate) const PROGRESS_IDLE_NOTICE_MAX: Duration = Duration::from_secs(30);
pub(crate) const PROGRESS_IDLE_NOTICE_MULTIPLIER: f64 = 1.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProgressCategory {
    Test,
    Edit,
    Read,
    Search,
    Terminal,
    Skill,
    Browser,
    Tool,
    Agent,
    Work,
}

impl ProgressCategory {
    fn label(self, language: &str) -> &'static str {
        if is_english(language) {
            match self {
                Self::Test => "tests",
                Self::Edit => "editing",
                Self::Read => "reading",
                Self::Search => "searching",
                Self::Terminal => "terminal",
                Self::Skill => "skills",
                Self::Browser => "browser",
                Self::Tool => "tools",
                Self::Agent => "agent",
                Self::Work => "work",
            }
        } else {
            match self {
                Self::Test => "테스트",
                Self::Edit => "수정",
                Self::Read => "읽기",
                Self::Search => "검색",
                Self::Terminal => "터미널",
                Self::Skill => "스킬",
                Self::Browser => "브라우저",
                Self::Tool => "도구",
                Self::Agent => "에이전트",
                Self::Work => "작업",
            }
        }
    }

    fn emoji(self) -> &'static str {
        match self {
            Self::Test => "🧪",
            Self::Edit => "✏️",
            Self::Read => "📖",
            Self::Search => "🔎",
            Self::Terminal => "💻",
            Self::Skill => "🧩",
            Self::Browser => "🌐",
            Self::Tool => "🛠️",
            Self::Agent => "🤖",
            Self::Work => "⚙️",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerboseProgressCommand {
    Enable,
    Disable,
}

impl VerboseProgressCommand {
    pub(crate) fn enabled(self) -> bool {
        matches!(self, Self::Enable)
    }
}

pub(crate) fn progress_category(event: &str, language: &str) -> ProgressCategory {
    let _ = language;
    let lower = event.trim().to_ascii_lowercase();
    let compact = lower
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();

    if contains_any(
        &lower,
        &["test", "pytest", "cargo test", "npm test", "node --test"],
    ) || contains_any(&compact, &["테스트", "검증"])
    {
        ProgressCategory::Test
    } else if contains_any(
        &lower,
        &[
            "edit",
            "write",
            "patch",
            "multiedit",
            "notebookedit",
            "apply_patch",
        ],
    ) || contains_any(&compact, &["수정", "쓰기", "변경", "패치"])
    {
        ProgressCategory::Edit
    } else if contains_any(&lower, &["read", "open file", "cat ", "sed "])
        || contains_any(&compact, &["읽기", "열람", "파일확인"])
    {
        ProgressCategory::Read
    } else if contains_any(
        &lower,
        &["search", "grep", "glob", "find", "websearch", "web_search"],
    ) || contains_any(&compact, &["검색", "찾기"])
    {
        ProgressCategory::Search
    } else if contains_any(
        &lower,
        &[
            "terminal", "shell", "bash", "command", "exec", "cargo ", "npm ",
        ],
    ) || contains_any(&compact, &["터미널", "명령", "실행"])
    {
        ProgressCategory::Terminal
    } else if contains_any(&lower, &["skill"]) || compact.contains("스킬") {
        ProgressCategory::Skill
    } else if contains_any(&lower, &["browser", "webfetch", "web_fetch"])
        || compact.contains("브라우저")
    {
        ProgressCategory::Browser
    } else if contains_any(&lower, &["tool:", "tool_result", "tool"])
        || contains_any(&compact, &["툴", "도구"])
    {
        ProgressCategory::Tool
    } else if contains_any(&lower, &["agent", "task", "thinking"]) || compact.contains("에이전트")
    {
        ProgressCategory::Agent
    } else {
        ProgressCategory::Work
    }
}

pub(crate) fn progress_detail(event: &str, language: &str) -> String {
    let trimmed = event.trim();
    let lower = trimmed.to_ascii_lowercase();
    let detail = if lower == "thinking" {
        if is_english(language) {
            "thinking".to_string()
        } else {
            "생각 중".to_string()
        }
    } else if lower == "agent:start" {
        if is_english(language) {
            "agent started".to_string()
        } else {
            "에이전트 시작".to_string()
        }
    } else if lower == "agent:done" {
        if is_english(language) {
            "agent done".to_string()
        } else {
            "에이전트 완료".to_string()
        }
    } else if lower == "tool_result:error" {
        if is_english(language) {
            "tool error".to_string()
        } else {
            "도구 오류".to_string()
        }
    } else if lower == "tool_result:ok" {
        if is_english(language) {
            "tool complete".to_string()
        } else {
            "도구 완료".to_string()
        }
    } else if let Some(label) = parse_progress_marker_line(trimmed) {
        label
    } else if let Some(rest) = trimmed.strip_prefix("tool:") {
        rest.replace(':', " ")
    } else {
        trimmed.to_string()
    };

    truncate_chars(&sanitize_detail(&detail), 56)
}

pub(crate) fn format_progress_message(event: &str, language: &str) -> String {
    let category = progress_category(event, language);
    let detail = progress_detail(event, language);
    if detail.trim().is_empty() {
        category.emoji().to_string()
    } else {
        format!("{} {}", category.emoji(), detail)
    }
}

pub(crate) fn summarize_progress_events(events: &[String], language: &str) -> String {
    let mut categories = Vec::new();
    let mut considered = 0usize;
    for event in events.iter().take(PROGRESS_BATCH_MAX_EVENTS) {
        considered += 1;
        let category = progress_category(event, language);
        if !categories.contains(&category) {
            categories.push(category);
        }
    }

    if categories.is_empty() {
        return idle_notice(language).to_string();
    }

    let visible = categories
        .iter()
        .take(PROGRESS_BATCH_MAX_CATEGORIES)
        .map(|category| category.label(language))
        .collect::<Vec<_>>();
    let extra = considered.saturating_sub(visible.len());
    if is_english(language) {
        if extra > 0 {
            format!(
                "Working on {} and {} more items.",
                visible.join(", "),
                extra
            )
        } else {
            format!("Working on {}.", visible.join(", "))
        }
    } else if extra > 0 {
        format!("{} 외 {}개 작업 중이야.", visible.join(", "), extra)
    } else {
        format!("{} 중이야.", visible.join(", "))
    }
}

pub(crate) fn idle_notice(language: &str) -> &'static str {
    if is_english(language) {
        "Still working."
    } else {
        "아직 작업 중이야."
    }
}

pub(crate) fn next_idle_notice_delay(current: Duration) -> Duration {
    let next = current.mul_f64(PROGRESS_IDLE_NOTICE_MULTIPLIER);
    next.min(PROGRESS_IDLE_NOTICE_MAX)
}

pub(crate) fn parse_progress_marker_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    trimmed
        .strip_prefix(PROGRESS_MARKER)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn extract_progress_markers(content: &str) -> (String, Vec<String>) {
    let mut cleaned = String::with_capacity(content.len());
    let mut markers = Vec::new();

    for segment in content.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        if let Some(marker) = parse_progress_marker_line(line) {
            markers.push(marker);
        } else {
            cleaned.push_str(segment);
        }
    }

    (cleaned, markers)
}

pub(crate) fn parse_verbose_progress_command(transcript: &str) -> Option<VerboseProgressCommand> {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    let compact = lower
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    let mentions_verbose = lower.contains("verbose")
        || lower.contains("progress")
        || compact.contains("진행공유")
        || compact.contains("진행모드")
        || compact.contains("상세진행")
        || compact.contains("버보스")
        || compact.contains("프로그레스");
    if !mentions_verbose {
        return None;
    }

    if contains_any(
        &lower,
        &["turn off", "disable", "off", "stop verbose", "quiet"],
    ) || contains_any(&compact, &["꺼", "끄", "중지", "비활성", "조용히", "그만"])
    {
        return Some(VerboseProgressCommand::Disable);
    }

    if contains_any(&lower, &["turn on", "enable", " on", "start verbose"])
        || contains_any(&compact, &["켜", "활성", "시작"])
    {
        return Some(VerboseProgressCommand::Enable);
    }

    None
}

pub(crate) fn is_turn_start_event(label: &str) -> bool {
    label.trim().eq_ignore_ascii_case("agent:start")
}

pub(crate) fn is_turn_done_event(label: &str) -> bool {
    label.trim().eq_ignore_ascii_case("agent:done")
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn is_english(language: &str) -> bool {
    language.trim().to_ascii_lowercase().starts_with("en")
}

fn sanitize_detail(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !matches!(ch, '`' | '*' | '#' | '_' | '[' | ']'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let take = max_chars.saturating_sub(1);
    let mut out = value.chars().take(take).collect::<String>();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_progress_events_to_categories() {
        assert_eq!(
            progress_category("tool:Read src/main.rs", "ko"),
            ProgressCategory::Read
        );
        assert_eq!(
            progress_category("tool:Edit src/main.rs", "ko"),
            ProgressCategory::Edit
        );
        assert_eq!(
            progress_category("tool:Grep verbose_progress", "ko"),
            ProgressCategory::Search
        );
        assert_eq!(
            progress_category("tool:Bash cargo test", "ko"),
            ProgressCategory::Test
        );
        assert_eq!(
            progress_category("VERBALCODING_PROGRESS: 스킬 사용 github", "ko"),
            ProgressCategory::Skill
        );
    }

    #[test]
    fn summarizes_at_most_three_categories_and_five_events() {
        let events = vec![
            "tool:Read a.rs".to_string(),
            "tool:Edit a.rs".to_string(),
            "tool:Grep pattern".to_string(),
            "tool:Bash cargo test".to_string(),
            "tool:Skill github".to_string(),
            "tool:Agent worker".to_string(),
        ];

        assert_eq!(
            summarize_progress_events(&events, "ko"),
            "읽기, 수정, 검색 외 2개 작업 중이야."
        );
    }

    #[test]
    fn parses_progress_marker_lines_and_removes_them_from_text() {
        let (cleaned, markers) =
            extract_progress_markers("응답 전\nVERBALCODING_PROGRESS: 파일 읽기 src/main.rs\n결과");

        assert_eq!(cleaned, "응답 전\n결과");
        assert_eq!(markers, vec!["파일 읽기 src/main.rs"]);
    }

    #[test]
    fn recognizes_verbose_progress_voice_commands() {
        assert_eq!(
            parse_verbose_progress_command("진행 공유 켜줘"),
            Some(VerboseProgressCommand::Enable)
        );
        assert_eq!(
            parse_verbose_progress_command("상세 진행 꺼"),
            Some(VerboseProgressCommand::Disable)
        );
        assert_eq!(
            parse_verbose_progress_command("turn on verbose progress"),
            Some(VerboseProgressCommand::Enable)
        );
        assert_eq!(
            parse_verbose_progress_command("disable verbose progress"),
            Some(VerboseProgressCommand::Disable)
        );
        assert_eq!(
            parse_verbose_progress_command("verbose progress status"),
            None
        );
        assert_eq!(parse_verbose_progress_command("진행 상황 알려줘"), None);
    }

    #[test]
    fn idle_notice_delay_caps_at_maximum() {
        assert_eq!(
            next_idle_notice_delay(PROGRESS_IDLE_NOTICE_INITIAL),
            Duration::from_secs(18)
        );
        assert_eq!(
            next_idle_notice_delay(Duration::from_secs(18)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_idle_notice_delay(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }
}
