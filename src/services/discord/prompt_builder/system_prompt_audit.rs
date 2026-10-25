//! Pure rendering for the explicit system-prompt inspection command.
use regex::{Captures, Regex};
use std::sync::LazyLock;

const BODY_LIMIT: usize = 2_000 - 8;

pub(crate) fn redact(input: &str) -> (String, usize) {
    static KEY: LazyLock<Result<Regex, regex::Error>> =
        LazyLock::new(|| Regex::new(r#"(--key(?:\s+|=))(?:"[^"]*"|'[^']*'|[^\s`]+)"#));
    static TOKEN: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| {
        Regex::new(
            r"(?i)\b(?:sk-[a-z0-9_-]+|gh[pousr]_[a-z0-9_]+|github_pat_[a-z0-9_]+|xox[a-z][-.][a-z0-9_.-]+)|[a-z0-9+/_-]{40,}={0,2}|[a-f0-9]{32,}",
        )
    });
    let (Ok(key), Ok(token)) = (&*KEY, &*TOKEN) else {
        return ("***".into(), 1);
    };
    let mut count = 0;
    let keys = key.replace_all(input, |c: &Captures<'_>| {
        count += 1;
        format!("{}***", &c[1])
    });
    let tokens = token.replace_all(&keys, |_: &Captures<'_>| {
        count += 1;
        "***"
    });
    (tokens.into_owned(), count)
}

pub(crate) fn neutralize_fences(input: &str) -> String {
    let mut output = String::new();
    let mut run = 0;
    for ch in input.chars() {
        if ch == '`' {
            if run == 2 {
                output.push('\u{200b}');
                run = 0;
            }
            run += 1;
        } else {
            run = 0;
        }
        output.push(ch);
    }
    output
}

pub(crate) fn code_blocks(input: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let (mut current, mut units) = (String::new(), 0);
    for line in input.split_inclusive('\n') {
        let line_units = line.encode_utf16().count();
        if units > 0 && units + line_units > BODY_LIMIT {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        for ch in line.chars() {
            if units + ch.len_utf16() > BODY_LIMIT {
                chunks.push(std::mem::take(&mut current));
                units = 0;
            }
            current.push(ch);
            units += ch.len_utf16();
        }
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
        .into_iter()
        .map(|body| format!("```\n{body}\n```"))
        .collect()
}

pub(crate) fn render(
    prompt: &str,
    session: &str,
    started: Option<&str>,
    anchors: usize,
    original: bool,
) -> Vec<String> {
    let (masked, redactions) = redact(prompt);
    let blocks = code_blocks(&neutralize_fences(&masked));
    let origin = if original { "원본" } else { "재구성" };
    let anchors = if anchors == 0 {
        "없음".into()
    } else {
        format!("{anchors}개")
    };
    let session: String = session.chars().take(8).collect();
    let mut messages = vec![format!(
        "-# 🧾 system prompt · 세션 {session} 시작 {} · {}자 · {}개 · 앵커 {anchors} · 가림 {redactions} · {origin}",
        started.unwrap_or("알 수 없음"),
        prompt.chars().count(),
        blocks.len(),
    )];
    messages.extend(blocks);
    messages
}

#[cfg(test)]
mod tests;
