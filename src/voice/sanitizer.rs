//! Sanitizers for spoken voice output.

const SPOKEN_RESULT_CHAR_LIMIT: usize = 900;

pub(crate) fn spoken_result_only(answer: &str, language: &str) -> String {
    let cleaned = clean_spoken_lines(answer);
    let cleaned = collapse_spoken_whitespace(&cleaned);
    if cleaned.is_empty() {
        return String::new();
    }

    let (mut spoken, truncated) = truncate_at_sentence_boundary(&cleaned, SPOKEN_RESULT_CHAR_LIMIT);
    if truncated {
        let notice = mirror_notice(language);
        if !spoken.ends_with(['.', '!', '?', '。', '！', '？', '…']) {
            spoken.push('.');
        }
        spoken.push(' ');
        spoken.push_str(notice);
    }
    spoken
}

fn clean_spoken_lines(answer: &str) -> String {
    let mut cleaned = Vec::new();
    let mut in_fence = false;

    for line in answer.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || is_noise_line(trimmed) {
            continue;
        }

        let spoken = strip_markdown_noise(trimmed);
        if !spoken.is_empty() {
            cleaned.push(spoken);
        }
    }

    cleaned.join(" ")
}

fn is_noise_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }

    let lower = line.to_ascii_lowercase();
    if line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("@@")
        || line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("$ ")
        || line.starts_with("> ")
        || lower.starts_with("stdout:")
        || lower.starts_with("stderr:")
        || lower.starts_with("output:")
        || lower.starts_with("logs:")
        || lower.starts_with("log:")
        || lower.starts_with("diff:")
        || lower.starts_with("changed files:")
        || lower.starts_with("verification log:")
        || lower.starts_with("test output:")
        || lower.starts_with("run:")
        || lower.starts_with("command:")
        || line.starts_with("변경 파일:")
        || line.starts_with("검증 로그:")
        || line.starts_with("테스트 출력:")
        || is_bare_command_line(&lower)
    {
        return true;
    }

    let mut chars = line.chars();
    matches!(chars.next(), Some('+') | Some('-')) && !matches!(chars.next(), Some(' '))
}

fn is_bare_command_line(lower: &str) -> bool {
    let mut parts = lower.split_whitespace();
    let Some(binary) = parts.next() else {
        return false;
    };
    let Some(subcommand) = parts.next() else {
        return false;
    };

    match binary {
        "cargo" => matches!(
            subcommand,
            "bench"
                | "build"
                | "check"
                | "clippy"
                | "doc"
                | "fmt"
                | "install"
                | "metadata"
                | "nextest"
                | "publish"
                | "run"
                | "test"
                | "tree"
                | "update"
        ),
        "git" => matches!(
            subcommand,
            "add"
                | "bisect"
                | "branch"
                | "checkout"
                | "cherry-pick"
                | "clone"
                | "commit"
                | "diff"
                | "fetch"
                | "log"
                | "merge"
                | "pull"
                | "push"
                | "rebase"
                | "remote"
                | "reset"
                | "restore"
                | "rev-parse"
                | "show"
                | "status"
                | "switch"
        ),
        _ => false,
    }
}

fn strip_markdown_noise(line: &str) -> String {
    let mut stripped = line.trim();
    stripped = stripped.trim_start_matches('#').trim();

    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            stripped = rest.trim();
            break;
        }
    }

    let mut output = stripped
        .replace("`", "")
        .replace("**", "")
        .replace("__", "")
        .replace("~~", "");
    output = output.replace('[', "").replace(']', "");
    output = remove_markdown_link_targets(&output);
    output.trim().to_string()
}

fn remove_markdown_link_targets(text: &str) -> String {
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '(' {
            let mut target = String::new();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == ')' {
                    break;
                }
                target.push(next);
            }
            if target.starts_with("http://") || target.starts_with("https://") {
                continue;
            }
            output.push('(');
            output.push_str(&target);
            output.push(')');
            continue;
        }
        output.push(ch);
    }
    output
}

fn collapse_spoken_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_at_sentence_boundary(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }

    let mut boundary_end = None;
    let mut hard_end = 0;
    for (idx, ch) in text.char_indices() {
        let end = idx + ch.len_utf8();
        if text[..end].chars().count() > max_chars {
            break;
        }
        hard_end = end;
        if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？' | '…') {
            boundary_end = Some(end);
        }
    }

    let end = boundary_end.unwrap_or(hard_end);
    (text[..end].trim().to_string(), true)
}

fn mirror_notice(language: &str) -> &'static str {
    if language.to_ascii_lowercase().starts_with("ko") {
        "나머지는 텍스트 채널에 남겼어."
    } else {
        "I left the rest in the text channel."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_code_blocks_and_diff_noise() {
        let spoken = spoken_result_only(
            "요약입니다.\n```rust\nfn main() {}\n```\ndiff --git a/a b/a\n@@ -1 +1 @@\n-old\n+new\n완료했습니다.",
            "ko",
        );

        assert_eq!(spoken, "요약입니다. 완료했습니다.");
    }

    #[test]
    fn strips_run_log_headers_and_markdown_markers() {
        let spoken = spoken_result_only(
            "## 결과\n- `cargo test` 통과\nstdout: noisy\n[문서](https://example.com)를 확인했어요.",
            "ko",
        );

        assert_eq!(spoken, "결과 cargo test 통과 문서를 확인했어요.");
    }

    #[test]
    fn removes_diff_and_verification_headers_from_voice_result() {
        let spoken = spoken_result_only(
            "완료했습니다.\ndiff: src/lib.rs\nchanged files: 2\nverification log: cargo test\n테스트 출력: noisy\n다음은 텍스트 채널에 남겼습니다.",
            "ko",
        );

        assert_eq!(spoken, "완료했습니다. 다음은 텍스트 채널에 남겼습니다.");
    }

    #[test]
    fn removes_bare_cargo_and_git_commands_without_dropping_korean_prose() {
        let spoken = spoken_result_only(
            "cargo test --all\ncargo 는 러스트 빌드 도구입니다.\ngit status\ngit 이 익숙하면 작업이 빨라져요.",
            "ko",
        );

        assert_eq!(
            spoken,
            "cargo 는 러스트 빌드 도구입니다. git 이 익숙하면 작업이 빨라져요."
        );
    }

    #[test]
    fn limits_long_spoken_result_and_attaches_korean_notice() {
        let long = "문장입니다. ".repeat(200);
        let spoken = spoken_result_only(&long, "ko-KR");

        assert!(spoken.chars().count() <= SPOKEN_RESULT_CHAR_LIMIT + 30);
        assert!(spoken.ends_with("나머지는 텍스트 채널에 남겼어."));
    }

    #[test]
    fn empty_after_sanitizing_stays_empty() {
        let spoken = spoken_result_only("```diff\n+added\n```", "ko");

        assert!(spoken.is_empty());
    }
}
