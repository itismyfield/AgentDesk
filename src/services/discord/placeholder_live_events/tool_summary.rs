//! Allowlisted, fail-closed details for the existing tool activity surface.
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

fn tool_key(name: &str) -> String {
    name.trim()
        .strip_prefix("functions.")
        .unwrap_or(name.trim())
        .to_ascii_lowercase()
}

pub(super) fn class(name: &str) -> &'static str {
    let key = tool_key(name);
    if key.starts_with("mcp__") {
        return "MCP";
    }
    match key.as_str() {
        "bash" | "shell" | "shell_command" | "command_execution" | "exec" | "exec_command"
        | "run_cmd" => "Bash",
        "read" | "read_file" | "read_text_file" => "Read",
        "edit" | "multiedit" | "multi_edit" | "notebookedit" | "apply_patch" => "Edit",
        "write" | "write_file" => "Write",
        "grep" | "rg" | "search_files" => "Grep",
        "glob" | "find_files" => "Glob",
        "task" | "agent" => "Task",
        "webfetch" | "web_fetch" => "WebFetch",
        "websearch" | "web_search" => "WebSearch",
        _ => "Tool",
    }
}

fn field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

// A whole shell word includes adjacent quoted/unquoted fragments. Matching a
// single quoted fragment would leave the tail of PASSWORD='head'"tail" visible.
const WORD: &str = r#"(?:'[^']*'|"[^"]*"|[^\s'"\\;&|<>])+"#;

fn redact_credentials(input: &str) -> String {
    static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(WORD).expect("shell words"));
    static ASSIGNED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(?:password|passwd|token|secret|[a-z0-9_-]+[_-]key|api[_-]?key|private[_-]?key)['"]?[ \t]*[:=][ \t]*"#).expect("assigned credential word")
    });
    static FLAGS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"(?i)((?:^|[\s;&|])(?:--(?:password|passwd|token|api[_-]?key|secret|client[_-]secret)(?:[ \t]*=[ \t]*|[ \t]+)|-p))({WORD})"
        )).expect("credential flags")
    });
    static ASSIGNMENTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r#"(?i)(\b(?:[a-z0-9_-]*(?:password|passwd|token|secret)[a-z0-9_-]*|[a-z0-9_-]+[_-]key|api[_-]?key|private[_-]?key)['"]?[ \t]*[:=][ \t]*)({WORD})"#
        )).expect("credential assignments")
    });
    static BARE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"(?i)(\b(?:password|passwd|token|api[_-]?key|secret|bearer|basic)[ \t]+)({WORD})"
        ))
        .expect("credential words")
    });
    // Whole-word replacement also covers `echo "token=head tail"`, where the
    // opening quote occurs before the key rather than before its value.
    let masked = WORDS.replace_all(input, |captures: &regex::Captures<'_>| {
        let word = captures.get(0).expect("whole word").as_str();
        if ASSIGNED
            .find(word)
            .is_some_and(|key| key.end() < word.len())
        {
            "***".to_string()
        } else {
            word.to_string()
        }
    });
    let masked = crate::utils::redact::redact_sensitive_headers(&masked, "***");
    let masked = FLAGS.replace_all(&masked, "${1}***");
    let masked = ASSIGNMENTS.replace_all(&masked, "${1}***");
    BARE.replace_all(&masked, "${1}***").into_owned()
}

fn redact_urls(input: &str) -> String {
    static USERINFO: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)([a-z][a-z0-9+.-]*://)[^\s/'"<>]*@"#).expect("URL userinfo")
    });
    static QUERY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)([?&](?:x-amz-[a-z0-9-]+|sig(?:nature)?|token|api[_-]?key|secret|password|passwd|access[_-]?token)=)[^&#\s'"<>]*"#).expect("signed URL query")
    });
    let masked = USERINFO.replace_all(input, "${1}***@");
    QUERY.replace_all(&masked, "${1}***").into_owned()
}

fn redact_tokens(input: &str) -> String {
    static KNOWN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:gh[pousr]_[a-z0-9_]+|github_pat_[a-z0-9_]+|sk-(?:ant-)?[a-z0-9_-]+|xox[bp]-[a-z0-9-]+|(?:AKIA|ASIA)[a-z0-9]+|AIza[a-z0-9_-]+|eyJ[a-z0-9_-]+(?:\.[a-z0-9_-]+){0,2})").expect("known token prefixes")
    });
    static OPAQUE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[a-zA-Z0-9+/_=-]{32,}").expect("opaque credential chunks"));
    let masked = KNOWN.replace_all(input, "***");
    OPAQUE.replace_all(&masked, "***").into_owned()
}

fn anonymize_home(input: &str) -> String {
    static HOME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:[a-z]:[/\\]users[/\\]|/(?:users|home)/)[^/\\\r\n]+")
            .expect("account home paths")
    });
    HOME.replace_all(input, "~").into_owned()
}

fn sanitized(raw: &str) -> Option<String> {
    // Existing sections have already escaped e.g. `ghp_` into `ghp\_`.
    // Inspect an unescaped probe and drop ambiguous escaped sensitive material.
    let probe = raw.replace('\\', "");
    let lower = probe.to_ascii_lowercase();
    let credential_syntax = [
        "password",
        "passwd",
        "token",
        "secret",
        "key",
        "authorization",
        "bearer",
        "basic",
    ]
    .iter()
    .any(|key| lower.contains(key));
    // Encoded or incomplete secret-bearing syntax cannot be safely summarized.
    if raw.contains('%')
        || raw.contains('\0')
        || (credential_syntax && !balanced_quotes(raw))
        || (raw.contains('\\')
            && (credential_syntax
                || redact_tokens(&probe) != probe
                || crate::utils::redact::contains_registered_secret(&probe)
                || ["/users/", "/home/", ":users", "://"]
                    .iter()
                    .any(|part| lower.contains(part))))
        || lower.contains("-----begin")
        || (lower.contains("security")
            && (lower.contains("find-generic-password")
                || lower.contains("find-internet-password")))
    {
        return None;
    }
    let masked = redact_credentials(raw);
    let masked = redact_urls(&masked);
    let masked = redact_tokens(&masked);
    let masked = crate::services::discord::formatting::redact_sensitive_for_placeholder(&masked);
    let masked = crate::utils::redact::redact_known_secrets(&masked);
    let masked = anonymize_home(&masked);
    // Recheck without quotes: quoted key/prefix fragments can hide sensitive
    // material. New matches after masking are ambiguous, so omit the detail.
    let dequoted = masked.replace(['\'', '"'], "");
    if dequoted != masked
        && (redact_credentials(&dequoted) != dequoted
            || redact_urls(&dequoted) != dequoted
            || redact_tokens(&dequoted) != dequoted
            || anonymize_home(&dequoted) != dequoted
            || crate::utils::redact::contains_registered_secret(&dequoted))
    {
        return None;
    }
    Some(masked)
}

/// Existing generated sections already escape Markdown; retain that structure
/// while applying the same credential and mention boundary to every line.
pub(super) fn safe_section(raw: &str) -> Option<String> {
    let masked = sanitized(raw)?;
    if masked
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return None;
    }
    Some(masked.replace('@', "@\u{200b}"))
}

fn text(raw: &str, limit: usize) -> Option<String> {
    let masked = sanitized(raw)?;
    let first = masked.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    let mut output = String::new();
    // Escape before truncating, keeping each escape pair and @-neutralizer whole.
    for ch in first.chars() {
        let unit = match ch {
            '@' => "@\u{200b}".to_string(),
            '\\' | '`' | '*' | '_' | '~' | '|' | '[' | ']' | '(' | ')' | '<' | '>' | '#' => {
                format!("\\{ch}")
            }
            ch if ch.is_control() => return None,
            _ => ch.to_string(),
        };
        if output.chars().count() + unit.chars().count() > limit {
            output.push('…');
            break;
        }
        output.push_str(&unit);
    }
    Some(output)
}

fn shell_text(raw: &str) -> Option<String> {
    // Expansions, escaping and heredocs require shell interpretation. Never
    // guess where their credential values end; ordinary quoted words are safe.
    if raw.contains(['$', '`', '\\']) || raw.contains("<<") {
        return None;
    }
    balanced_quotes(raw).then(|| text(raw, 80)).flatten()
}

fn balanced_quotes(raw: &str) -> bool {
    let mut quote = None;
    for ch in raw.chars() {
        match (quote, ch) {
            (Some(open), close) if open == close => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            _ => {}
        }
    }
    quote.is_none()
}

fn normalized(path: &str) -> String {
    let path = path.replace('\\', "/");
    let absolute = path.starts_with('/');
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            _ => parts.push(part),
        }
    }
    format!("{}{}", if absolute { "/" } else { "" }, parts.join("/"))
}

fn private_path(path: &str) -> bool {
    path.replace('\\', "/").split('/').any(|part| {
        let part = part.to_ascii_lowercase();
        part.starts_with(".env")
            || matches!(part.as_str(), ".ssh" | ".gnupg" | "id_rsa" | "id_ed25519")
            || part.ends_with(".pem")
            || part.ends_with(".key")
            || ["credential", "secret", "keychain"]
                .iter()
                .any(|needle| part.contains(needle))
    })
}

fn path_summary(raw: &str, cwd: Option<&str>) -> Option<String> {
    if raw.is_empty() || raw.contains(['\n', '\r', '\0', '%']) {
        return None;
    }
    if private_path(raw) {
        return Some("[비공개 파일]".into());
    }
    let path = normalized(raw);
    let absolute = path.starts_with('/') || path.as_bytes().get(1) == Some(&b':');
    let cwd = cwd.map(normalized);
    let resolved = if !absolute {
        cwd.as_ref()
            .map(|cwd| normalized(&format!("{cwd}/{path}")))
            .unwrap_or_else(|| path.clone())
    } else {
        path.clone()
    };
    if private_path(&resolved) {
        return Some("[비공개 파일]".into());
    }
    let relative = cwd.as_ref().and_then(|cwd| {
        resolved
            .strip_prefix(&format!("{cwd}/"))
            .or_else(|| (resolved == *cwd).then_some("."))
    });
    let visible = if let Some(relative) = relative {
        relative.to_string()
    } else if !absolute && !path.starts_with("../") && cwd.is_none() {
        path
    } else {
        let anonymous = anonymize_home(&resolved);
        let parts: Vec<_> = anonymous
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        format!("…/{}", parts[parts.len().saturating_sub(2)..].join("/"))
    };
    text(&visible, 120)
}

fn edit_counts(value: &Value) -> Option<(usize, usize)> {
    if let Some(edits) = value.get("edits").and_then(Value::as_array) {
        return edits.iter().try_fold((0, 0), |(added, removed), edit| {
            let (a, r) = edit_counts(edit)?;
            Some((added + a, removed + r))
        });
    }
    Some((
        value.get("new_string")?.as_str()?.lines().count(),
        value.get("old_string")?.as_str()?.lines().count(),
    ))
}

fn patch_summary(input: &str, cwd: Option<&str>) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(input).ok();
    let patch = parsed
        .as_ref()
        .and_then(|value| value.as_str().or_else(|| field(value, &["patch", "input"])))
        .unwrap_or(input);
    if !patch.starts_with("*** Begin Patch\n") || !patch.trim_end().ends_with("*** End Patch") {
        return None;
    }
    let mut target = None;
    let (mut added, mut removed) = (0, 0);
    for line in patch.lines() {
        for prefix in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            if let Some(path) = line.strip_prefix(prefix) {
                target.get_or_insert(path);
            }
        }
        added += usize::from(line.starts_with('+'));
        removed += usize::from(line.starts_with('-'));
    }
    let path = path_summary(target?, cwd)?;
    Some(format!("{path} (+{added} −{removed})"))
}

pub(super) fn safe_tool_summary(name: &str, input: &str, cwd: Option<&str>) -> Option<String> {
    if input.len() > 262_144 {
        return None;
    }
    let key = tool_key(name);
    if class(name) == "MCP" {
        let original = name
            .trim()
            .strip_prefix("functions.")
            .unwrap_or(name.trim());
        let (server, tool) = original.get(5..)?.split_once("__")?;
        if [server, tool].iter().any(|part| {
            part.is_empty()
                || part.len() > 80
                || !part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        }) {
            return None;
        }
        return text(&format!("{server} · {tool}"), 100);
    }
    if key == "apply_patch" {
        return patch_summary(input, cwd);
    }
    let value = super::status_events::tool_input_value(input);
    if !value.is_object() {
        return None;
    }
    let cwd = field(&value, &["workdir", "cwd"]).or(cwd);
    match class(name) {
        "Bash" => shell_text(field(&value, &["command", "cmd"])?),
        "Read" | "Edit" | "Write" => {
            let path = path_summary(field(&value, &["file_path", "path", "notebook_path"])?, cwd)?;
            Some(match edit_counts(&value) {
                Some((added, removed)) => format!("{path} (+{added} −{removed})"),
                None => path,
            })
        }
        "Grep" | "Glob" => {
            let pattern = text(field(&value, &["pattern", "query"])?, 50)?;
            match field(&value, &["path", "cwd"]) {
                Some(path) => Some(format!("\"{pattern}\" · {}", path_summary(path, cwd)?)),
                None => Some(format!("\"{pattern}\"")),
            }
        }
        "Task" => text(field(&value, &["description"])?, 60),
        "WebSearch" => text(field(&value, &["query", "search_term"])?, 60),
        "WebFetch" => {
            let url = url::Url::parse(field(&value, &["url"])?).ok()?;
            if !matches!(url.scheme(), "http" | "https") {
                return None;
            }
            text(&format!("{}{}", url.host_str()?, url.path()), 100)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "tool_summary_tests.rs"]
mod tool_summary_tests;
